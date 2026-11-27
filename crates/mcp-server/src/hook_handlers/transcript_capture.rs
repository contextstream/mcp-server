//! Coordinated, race-safe session transcript capture for lifecycle hooks
//! (`UserPromptSubmit` and `Stop`).
//!
//! Each user prompt and assistant reply is prepared verbatim (trimmed,
//! secret-scrubbed, capped at 16 KiB on a UTF-8 boundary, never enriched with
//! derived parenthetical context), durably enqueued under a fast per-session
//! file lock, and drained to `/api/v1/transcripts` in strict FIFO order under a
//! separate per-session drain lock.
//!
//! Keeping enqueue and drain separate guarantees:
//! - Concurrent `UserPromptSubmit` and `Stop` processes never race to create
//!   duplicate transcripts or drop each other's messages.
//! - Transient API errors and timeouts retry within the hook's budget, and any
//!   still-undelivered message stays queued (with its error recorded to local
//!   state and stderr) so the next hook drains it in order before later turns.
//! - Idempotency keys deduplicate replayed events and retries without ever
//!   collapsing two distinct prompts that happen to share identical text or
//!   arrive in the same second.

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::common::{
    extract_id, extract_items, host_session_key, prefix_within_bytes, scrub_credential_tokens,
    scrub_setup_secrets, transcript_has_account_setup, TranscriptHookConfig,
};
use super::durable_capture::{
    plain_session_id, sha256_hex, state_dir, with_locked_json, with_locked_json_within,
};

/// Maximum UTF-8 byte length kept for a transcript message appended from hooks.
pub(crate) const MAX_TRANSCRIPT_MESSAGE_BYTES: usize = 16 * 1024;
const TRUNCATION_SUFFIX: &str = "...";

/// Hook budgets: `UserPromptSubmit` has a 5 s Claude Code timeout; `Stop` has
/// a 15 s timeout. Each stays comfortably inside its ceiling even under retry.
pub(crate) const PROMPT_CAPTURE_BUDGET: Duration = Duration::from_millis(3_800);
pub(crate) const STOP_CAPTURE_BUDGET: Duration = Duration::from_millis(8_000);

const PER_REQUEST_TIMEOUT_CAP: Duration = Duration::from_secs(3);
const MIN_REQUEST_TIMEOUT: Duration = Duration::from_millis(400);
const MAX_ATTEMPTS_PER_HOOK: u32 = 3;
const MAX_PERSISTENT_ATTEMPTS: u32 = 12;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(120);
const STATE_LOCK_WAIT: Duration = Duration::from_millis(1_000);
const DRAIN_LOCK_WAIT_CAP: Duration = Duration::from_millis(3_200);

const MAX_PENDING_PER_SESSION: usize = 64;
const MAX_DELIVERED_KEYS_PER_SESSION: usize = 512;
const MAX_FAILURES_PER_SESSION: usize = 32;
const SESSION_STATE_MAX_AGE_HOURS: i64 = 24;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionTranscriptState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript_session_id: Option<String>,
    #[serde(default)]
    next_seq: u64,
    #[serde(default)]
    pending: Vec<PendingTranscriptMessage>,
    #[serde(default)]
    delivered_keys: Vec<String>,
    #[serde(default)]
    failures: Vec<RecordedFailure>,
    #[serde(default)]
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingTranscriptMessage {
    seq: u64,
    idempotency_key: String,
    role: String,
    content: String,
    timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    #[serde(default)]
    attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordedFailure {
    idempotency_key: String,
    role: String,
    attempts: u32,
    error: String,
    failed_at: String,
}

#[derive(Debug)]
enum ApiAttemptError {
    NotFound,
    Retryable(String),
    Permanent(String),
}

impl ApiAttemptError {
    fn message(&self) -> &str {
        match self {
            ApiAttemptError::NotFound => "HTTP 404 Not Found",
            ApiAttemptError::Retryable(msg) | ApiAttemptError::Permanent(msg) => msg.as_str(),
        }
    }

    fn is_retryable(&self) -> bool {
        matches!(self, ApiAttemptError::Retryable(_))
    }
}

/// Whether `prompt` is a harness-generated task notification rather than words
/// typed by the user.
pub(crate) fn is_harness_notification(prompt: &str) -> bool {
    prompt.contains("<task-notification>")
        || prompt.contains("SYSTEM NOTIFICATION - NOT USER INPUT")
}

/// Extract the user's verbatim prompt from a `UserPromptSubmit` hook payload.
/// Never enriches or rewrites the text; skips blank inputs and harness
/// notifications.
pub(crate) fn extract_verbatim_user_prompt(input: &Value) -> Option<String> {
    let raw = input
        .get("prompt")
        .or_else(|| input.get("user_prompt"))
        .or_else(|| input.get("user_message"))
        .or_else(|| input.get("message"))
        .and_then(Value::as_str)?;
    if is_harness_notification(raw) {
        return None;
    }
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Scrub secrets and cap a transcript message (prompt or assistant reply) at
/// 16 KiB on a UTF-8 character boundary. Preserves `b41b705` behaviour.
pub(crate) fn prepare_transcript_message(text: &str, input: &Value) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let setup_flow = input
        .get("transcript_path")
        .or_else(|| input.get("transcriptPath"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|raw| transcript_has_account_setup(&raw));

    let scrubbed = if setup_flow {
        scrub_setup_secrets(trimmed)
    } else {
        scrub_credential_tokens(trimmed)
    };
    let scrubbed = scrubbed.trim();
    if scrubbed.is_empty() {
        return None;
    }

    Some(cap_transcript_message(
        scrubbed,
        MAX_TRANSCRIPT_MESSAGE_BYTES,
    ))
}

/// Bound `text` to at most `max_bytes` UTF-8 bytes, cutting on a character
/// boundary and appending `...` when truncated.
pub(crate) fn cap_transcript_message(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes <= TRUNCATION_SUFFIX.len() {
        return prefix_within_bytes(text, max_bytes).to_string();
    }
    let prefix = prefix_within_bytes(text, max_bytes - TRUNCATION_SUFFIX.len());
    format!("{prefix}{TRUNCATION_SUFFIX}")
}

/// Session IDs to query for this conversation's transcript, preferring the
/// ContextStream API session ID recorded when `init` ran over the host's own
/// session ID.
pub(crate) fn candidate_session_ids(input: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(host_session) = host_session_key(input) {
        if let Some(api_session) = super::durable_capture::session_api_session_id(&host_session) {
            if api_session != "unknown" {
                ids.push(api_session);
            }
        }
        if host_session != "unknown" && !ids.iter().any(|existing| existing == &host_session) {
            ids.push(host_session);
        }
    }
    ids
}

/// Build an idempotency key that deduplicates replayed deliveries of the same
/// hook event and internal retries, while never collapsing two distinct
/// prompts (even identical short prompts like `"continue"` typed on separate
/// turns, or prompts submitted within the same second).
fn message_idempotency_key(input: &Value, role: &str, content: &str) -> String {
    let explicit_id = [
        "idempotency_key",
        "event_id",
        "hook_invocation_id",
        "message_id",
        "turn_id",
        "request_id",
    ]
    .iter()
    .find_map(|key| {
        input
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    });

    let content_hash = sha256_hex(content.as_bytes());
    match explicit_id {
        Some(id) => format!("explicit:{role}:{id}:{content_hash}"),
        None => format!(
            "invoke:{role}:{}:{}",
            uuid::Uuid::new_v4(),
            &content_hash[..16]
        ),
    }
}

fn session_state_path(host_session: &str) -> Option<PathBuf> {
    let dir = state_dir()?.join("transcript-hook-state");
    Some(dir.join(format!("{}.json", sha256_hex(host_session.as_bytes()))))
}

fn drain_lock_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("drain.lock")
}

fn acquire_drain_lock(lock_path: &Path, wait: Duration) -> Option<File> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    let deadline = Instant::now() + wait;
    while fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Some(lock)
}

fn prune_session_state(state: &mut SessionTranscriptState, now: DateTime<Utc>) {
    let stale = !state.updated_at.is_empty()
        && DateTime::parse_from_rfc3339(&state.updated_at).is_ok_and(|ts| {
            now.signed_duration_since(ts.with_timezone(&Utc))
                > chrono::Duration::hours(SESSION_STATE_MAX_AGE_HOURS)
        });
    if stale {
        *state = SessionTranscriptState::default();
    }
    if state.delivered_keys.len() > MAX_DELIVERED_KEYS_PER_SESSION {
        let excess = state.delivered_keys.len() - MAX_DELIVERED_KEYS_PER_SESSION;
        state.delivered_keys.drain(..excess);
    }
    if state.failures.len() > MAX_FAILURES_PER_SESSION {
        let excess = state.failures.len() - MAX_FAILURES_PER_SESSION;
        state.failures.drain(..excess);
    }
}

fn enqueue_message(
    state_path: &Path,
    role: &str,
    content: String,
    idempotency_key: String,
    timestamp: String,
    client_name: Option<String>,
) -> bool {
    let now = Utc::now();
    with_locked_json_within(
        state_path,
        STATE_LOCK_WAIT,
        |state: &mut SessionTranscriptState| {
            prune_session_state(state, now);
            if state
                .delivered_keys
                .iter()
                .any(|key| key == &idempotency_key)
                || state
                    .pending
                    .iter()
                    .any(|item| item.idempotency_key == idempotency_key)
            {
                return false;
            }
            let seq = state.next_seq;
            state.next_seq = state.next_seq.saturating_add(1);
            state.pending.push(PendingTranscriptMessage {
                seq,
                idempotency_key,
                role: role.to_string(),
                content,
                timestamp,
                client_name,
                attempts: 0,
                last_error: None,
            });
            while state.pending.len() > MAX_PENDING_PER_SESSION {
                let evicted = state.pending.remove(0);
                let msg = "evicted oldest pending transcript message: queue full";
                eprintln!(
                    "contextstream transcript hook: {msg} (role={}, seq={})",
                    evicted.role, evicted.seq
                );
                state.failures.push(RecordedFailure {
                    idempotency_key: evicted.idempotency_key,
                    role: evicted.role,
                    attempts: evicted.attempts,
                    error: msg.to_string(),
                    failed_at: now.to_rfc3339(),
                });
            }
            state.updated_at = now.to_rfc3339();
            true
        },
    )
    .unwrap_or(false)
}

/// Append a verbatim user prompt from `UserPromptSubmit` to the session
/// transcript. Creates the transcript on the first prompt if none exists yet,
/// retries transient failures, and records any persistent failure.
pub(crate) async fn append_user_prompt(
    config: &TranscriptHookConfig,
    input: &Value,
    client_name: Option<&str>,
) {
    let Some(raw_prompt) = extract_verbatim_user_prompt(input) else {
        return;
    };
    let Some(prepared) = prepare_transcript_message(&raw_prompt, input) else {
        return;
    };
    record_and_drain(
        config,
        input,
        "user",
        prepared,
        client_name.map(str::to_string),
        PROMPT_CAPTURE_BUDGET,
    )
    .await;
}

/// Append a final assistant reply from `Stop` to the session transcript.
pub(crate) async fn append_assistant_reply(
    config: &TranscriptHookConfig,
    input: &Value,
    prepared_reply: &str,
) {
    record_and_drain(
        config,
        input,
        "assistant",
        prepared_reply.to_string(),
        None,
        STOP_CAPTURE_BUDGET,
    )
    .await;
}

async fn record_and_drain(
    config: &TranscriptHookConfig,
    input: &Value,
    role: &str,
    content: String,
    client_name: Option<String>,
    budget: Duration,
) {
    let candidates = candidate_session_ids(input);
    let Some(host_session) = host_session_key(input) else {
        return;
    };
    if candidates.is_empty() || config.api_key.is_empty() {
        return;
    }

    let idempotency_key = message_idempotency_key(input, role, &content);
    let timestamp = Utc::now().to_rfc3339();

    let Some(path) = session_state_path(&host_session) else {
        // Fallback when HOME is unavailable: direct best-effort attempt with retry.
        let deadline = Instant::now() + budget;
        let _ = direct_send_without_state(
            config,
            &candidates,
            role,
            &content,
            &timestamp,
            &idempotency_key,
            client_name.as_deref(),
            deadline,
        )
        .await;
        return;
    };

    enqueue_message(
        &path,
        role,
        content,
        idempotency_key,
        timestamp,
        client_name,
    );

    let deadline = Instant::now() + budget;
    let lock_wait = budget.min(DRAIN_LOCK_WAIT_CAP);
    let lock_path = drain_lock_path(&path);
    let Some(_drain_lock) = acquire_drain_lock(&lock_path, lock_wait) else {
        // Another hook process for this session is currently draining; our
        // message is already durably in `state.pending` and will be picked up
        // before that drainer finishes or by the next hook.
        return;
    };

    drain_pending_messages(config, &path, &candidates, deadline).await;
}

fn read_state_snapshot(state_path: &Path) -> SessionTranscriptState {
    std::fs::read(state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn mark_delivered(
    state_path: &Path,
    idempotency_key: &str,
    resolved_transcript: Option<(String, String)>,
) {
    let now = Utc::now();
    let _ = with_locked_json(state_path, |state: &mut SessionTranscriptState| {
        if let Some((tid, sid)) = resolved_transcript {
            state.transcript_id = Some(tid);
            state.transcript_session_id = Some(sid);
        }
        state
            .pending
            .retain(|item| item.idempotency_key != idempotency_key);
        if !state
            .delivered_keys
            .iter()
            .any(|key| key == idempotency_key)
        {
            state.delivered_keys.push(idempotency_key.to_string());
        }
        state.updated_at = now.to_rfc3339();
        prune_session_state(state, now);
    });
}

fn cache_transcript_id(state_path: &Path, transcript_id: String, session_id: String) {
    let now = Utc::now();
    let _ = with_locked_json(state_path, |state: &mut SessionTranscriptState| {
        state.transcript_id = Some(transcript_id);
        state.transcript_session_id = Some(session_id);
        state.updated_at = now.to_rfc3339();
    });
}

fn invalidate_cached_transcript_id(state_path: &Path) {
    let now = Utc::now();
    let _ = with_locked_json(state_path, |state: &mut SessionTranscriptState| {
        state.transcript_id = None;
        state.transcript_session_id = None;
        state.updated_at = now.to_rfc3339();
    });
}

fn record_delivery_failure(state_path: &Path, idempotency_key: &str, error: &str, permanent: bool) {
    let now = Utc::now();
    let _ = with_locked_json(state_path, |state: &mut SessionTranscriptState| {
        if let Some(pos) = state
            .pending
            .iter()
            .position(|item| item.idempotency_key == idempotency_key)
        {
            state.pending[pos].attempts = state.pending[pos].attempts.saturating_add(1);
            state.pending[pos].last_error = Some(error.to_string());
            let attempts = state.pending[pos].attempts;
            let role = state.pending[pos].role.clone();
            let seq = state.pending[pos].seq;
            eprintln!(
                "contextstream transcript hook: failed to persist {role} message (seq={seq}, attempts={attempts}): {error}"
            );
            state.failures.push(RecordedFailure {
                idempotency_key: idempotency_key.to_string(),
                role,
                attempts,
                error: error.to_string(),
                failed_at: now.to_rfc3339(),
            });
            if permanent || attempts >= MAX_PERSISTENT_ATTEMPTS {
                state.pending.remove(pos);
            }
            state.updated_at = now.to_rfc3339();
            prune_session_state(state, now);
        }
    });
}

async fn drain_pending_messages(
    config: &TranscriptHookConfig,
    state_path: &Path,
    candidates: &[String],
    deadline: Instant,
) {
    let mut refreshed_after_404 = false;

    loop {
        if Instant::now() >= deadline {
            break;
        }

        let snapshot = read_state_snapshot(state_path);
        if snapshot.pending.is_empty() {
            break;
        }

        let mut transcript_id = snapshot.transcript_id.clone();

        if transcript_id.is_none() {
            match lookup_existing_transcript(config, candidates, deadline).await {
                Ok(Some((tid, sid))) => {
                    cache_transcript_id(state_path, tid.clone(), sid);
                    transcript_id = Some(tid);
                }
                Ok(None) => {
                    // No transcript exists on the server yet. If there is a
                    // pending user prompt (even if a racing Stop reply arrived
                    // milliseconds earlier), create the transcript with the
                    // earliest user prompt first so user precedes assistant.
                    let Some(user_msg) = snapshot
                        .pending
                        .iter()
                        .find(|item| item.role == "user")
                        .cloned()
                    else {
                        // Only assistant message(s) pending and no transcript
                        // exists yet (preserve b41b705: never create an orphan
                        // assistant-only transcript; keep pending in case a
                        // concurrent UserPromptSubmit is right behind).
                        break;
                    };

                    let create_session = candidates[0].as_str();
                    match create_transcript_with_retries(
                        config,
                        create_session,
                        &user_msg,
                        deadline,
                    )
                    .await
                    {
                        Ok(created_id) => {
                            let resolved = created_id.map(|id| (id, create_session.to_string()));
                            mark_delivered(state_path, &user_msg.idempotency_key, resolved);
                            refreshed_after_404 = false;
                            continue;
                        }
                        Err(err) => {
                            record_delivery_failure(
                                state_path,
                                &user_msg.idempotency_key,
                                err.message(),
                                !err.is_retryable(),
                            );
                            break;
                        }
                    }
                }
                Err(err) => {
                    if let Some(first) = snapshot.pending.first() {
                        record_delivery_failure(
                            state_path,
                            &first.idempotency_key,
                            err.message(),
                            !err.is_retryable(),
                        );
                    }
                    break;
                }
            }
        }

        let Some(tid) = transcript_id else {
            break;
        };

        let next = snapshot.pending[0].clone();
        match append_message_with_retries(config, &tid, &next, deadline).await {
            Ok(()) => {
                mark_delivered(state_path, &next.idempotency_key, None);
                refreshed_after_404 = false;
            }
            Err(ApiAttemptError::NotFound) if !refreshed_after_404 => {
                invalidate_cached_transcript_id(state_path);
                refreshed_after_404 = true;
            }
            Err(err) => {
                record_delivery_failure(
                    state_path,
                    &next.idempotency_key,
                    err.message(),
                    !err.is_retryable(),
                );
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn direct_send_without_state(
    config: &TranscriptHookConfig,
    candidates: &[String],
    role: &str,
    content: &str,
    timestamp: &str,
    idempotency_key: &str,
    client_name: Option<&str>,
    deadline: Instant,
) -> Result<(), ApiAttemptError> {
    let msg = PendingTranscriptMessage {
        seq: 0,
        idempotency_key: idempotency_key.to_string(),
        role: role.to_string(),
        content: content.to_string(),
        timestamp: timestamp.to_string(),
        client_name: client_name.map(str::to_string),
        attempts: 0,
        last_error: None,
    };
    match lookup_existing_transcript(config, candidates, deadline).await? {
        Some((tid, _)) => append_message_with_retries(config, &tid, &msg, deadline).await,
        None if role == "user" => {
            create_transcript_with_retries(config, &candidates[0], &msg, deadline)
                .await
                .map(|_| ())
        }
        None => Ok(()),
    }
}

fn remaining_request_timeout(deadline: Instant) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    if remaining < MIN_REQUEST_TIMEOUT {
        return None;
    }
    Some(remaining.min(PER_REQUEST_TIMEOUT_CAP))
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

async fn run_with_retries<T, F, Fut>(deadline: Instant, mut op: F) -> Result<T, ApiAttemptError>
where
    F: FnMut(Duration) -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiAttemptError>>,
{
    let mut backoff = INITIAL_RETRY_BACKOFF;
    let mut last_err = ApiAttemptError::Retryable("transcript hook budget exhausted".to_string());

    for attempt in 0..MAX_ATTEMPTS_PER_HOOK {
        let Some(timeout) = remaining_request_timeout(deadline) else {
            return Err(last_err);
        };
        match op(timeout).await {
            Ok(val) => return Ok(val),
            Err(err) => {
                let retryable = err.is_retryable();
                last_err = err;
                if !retryable || attempt + 1 >= MAX_ATTEMPTS_PER_HOOK {
                    return Err(last_err);
                }
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return Err(last_err);
                };
                if remaining <= backoff + MIN_REQUEST_TIMEOUT {
                    return Err(last_err);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_millis(600));
            }
        }
    }

    Err(last_err)
}

async fn lookup_existing_transcript(
    config: &TranscriptHookConfig,
    candidates: &[String],
    deadline: Instant,
) -> Result<Option<(String, String)>, ApiAttemptError> {
    for session_id in candidates {
        if let Some(tid) = find_session_transcript_id(config, session_id, deadline).await? {
            return Ok(Some((tid, session_id.clone())));
        }
    }
    Ok(None)
}

async fn find_session_transcript_id(
    config: &TranscriptHookConfig,
    session_id: &str,
    deadline: Instant,
) -> Result<Option<String>, ApiAttemptError> {
    run_with_retries(deadline, |timeout| async move {
        let mut query = vec![("session_id", session_id), ("per_page", "1")];
        if let Some(ref ws_id) = config.workspace_id {
            query.push(("workspace_id", ws_id.as_str()));
        }
        if let Some(ref proj_id) = config.project_id {
            query.push(("project_id", proj_id.as_str()));
        }

        let response = super::api_http_client()
            .get(format!("{}/api/v1/transcripts", config.api_url))
            .header("X-API-Key", &config.api_key)
            .query(&query)
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| ApiAttemptError::Retryable(format!("GET /transcripts failed: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            let msg = format!("GET /transcripts returned HTTP {}", status.as_u16());
            return if is_retryable_status(status) {
                Err(ApiAttemptError::Retryable(msg))
            } else {
                Err(ApiAttemptError::Permanent(msg))
            };
        }

        let value: Value = response.json().await.map_err(|err| {
            ApiAttemptError::Retryable(format!("GET /transcripts invalid JSON: {err}"))
        })?;
        let items = extract_items(&value);
        Ok(items
            .first()
            .and_then(extract_id)
            .and_then(|id| plain_session_id(&id)))
    })
    .await
}

async fn create_transcript_with_retries(
    config: &TranscriptHookConfig,
    session_id: &str,
    first_message: &PendingTranscriptMessage,
    deadline: Instant,
) -> Result<Option<String>, ApiAttemptError> {
    let mut payload = serde_json::json!({
        "session_id": session_id,
        "source_type": "hook",
        "started_at": first_message.timestamp,
        "messages": [{
            "role": first_message.role,
            "content": first_message.content,
            "timestamp": first_message.timestamp,
        }],
    });
    if let Some(obj) = payload.as_object_mut() {
        if let Some(ref ws_id) = config.workspace_id {
            obj.insert("workspace_id".to_string(), Value::String(ws_id.clone()));
        }
        if let Some(ref proj_id) = config.project_id {
            obj.insert("project_id".to_string(), Value::String(proj_id.clone()));
        }
        if let Some(ref client_name) = first_message.client_name {
            obj.insert(
                "client_name".to_string(),
                Value::String(client_name.clone()),
            );
        }
    }

    run_with_retries(deadline, |timeout| {
        let payload = payload.clone();
        let idempotency_key = first_message.idempotency_key.clone();
        async move {
            let response = super::api_http_client()
                .post(format!("{}/api/v1/transcripts", config.api_url))
                .header("X-API-Key", &config.api_key)
                .header("Content-Type", "application/json")
                .header("X-Idempotency-Key", &idempotency_key)
                .json(&payload)
                .timeout(timeout)
                .send()
                .await
                .map_err(|err| {
                    ApiAttemptError::Retryable(format!("POST /transcripts failed: {err}"))
                })?;

            let status = response.status();
            if !status.is_success() {
                let msg = format!("POST /transcripts returned HTTP {}", status.as_u16());
                return if is_retryable_status(status) {
                    Err(ApiAttemptError::Retryable(msg))
                } else {
                    Err(ApiAttemptError::Permanent(msg))
                };
            }

            let created_id = response
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| extract_id(&v))
                .and_then(|id| plain_session_id(&id));
            Ok(created_id)
        }
    })
    .await
}

async fn append_message_with_retries(
    config: &TranscriptHookConfig,
    transcript_id: &str,
    message: &PendingTranscriptMessage,
    deadline: Instant,
) -> Result<(), ApiAttemptError> {
    let payload = serde_json::json!({
        "role": message.role,
        "content": message.content,
        "timestamp": message.timestamp,
    });

    run_with_retries(deadline, |timeout| {
        let payload = payload.clone();
        let idempotency_key = message.idempotency_key.clone();
        async move {
            let response = super::api_http_client()
                .post(format!(
                    "{}/api/v1/transcripts/{}/messages",
                    config.api_url, transcript_id
                ))
                .header("X-API-Key", &config.api_key)
                .header("Content-Type", "application/json")
                .header("X-Idempotency-Key", &idempotency_key)
                .json(&payload)
                .timeout(timeout)
                .send()
                .await
                .map_err(|err| {
                    ApiAttemptError::Retryable(format!(
                        "POST /transcripts/{transcript_id}/messages failed: {err}"
                    ))
                })?;

            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            if status == StatusCode::NOT_FOUND {
                return Err(ApiAttemptError::NotFound);
            }
            let msg = format!(
                "POST /transcripts/{transcript_id}/messages returned HTTP {}",
                status.as_u16()
            );
            if is_retryable_status(status) {
                Err(ApiAttemptError::Retryable(msg))
            } else {
                Err(ApiAttemptError::Permanent(msg))
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI_BYTE_CHARS: [&str; 3] = ["\u{1F600}", "\u{4E16}", "\u{E9}"];

    #[test]
    fn cap_transcript_message_keeps_short_text_and_bounds_long_utf8() {
        assert_eq!(cap_transcript_message("short reply", 100), "short reply");
        assert_eq!(cap_transcript_message("exact", 5), "exact");

        for ch in MULTI_BYTE_CHARS {
            let repeated = ch.repeat(20);
            for limit in 4..20 {
                let capped = cap_transcript_message(&repeated, limit);
                assert!(capped.len() <= limit, "limit {limit} for {ch:?}");
                assert!(capped.ends_with("..."), "limit {limit} for {ch:?}");
            }
        }
    }

    #[test]
    fn extract_verbatim_user_prompt_preserves_exact_text_and_skips_notifications() {
        let input = serde_json::json!({ "prompt": "  What does it tell us?  " });
        assert_eq!(
            extract_verbatim_user_prompt(&input).as_deref(),
            Some("What does it tell us?")
        );
        assert_eq!(
            extract_verbatim_user_prompt(&serde_json::json!({
                "prompt": "<task-notification><status>completed</status></task-notification>"
            })),
            None
        );
        assert_eq!(
            extract_verbatim_user_prompt(&serde_json::json!({ "prompt": "   \n " })),
            None
        );
    }

    #[test]
    fn idempotency_key_dedupes_explicit_replay_without_collapsing_distinct_prompts() {
        let replay = serde_json::json!({ "event_id": "evt-1" });
        assert_eq!(
            message_idempotency_key(&replay, "user", "check logs"),
            message_idempotency_key(&replay, "user", "check logs")
        );
        assert_ne!(
            message_idempotency_key(&replay, "user", "check logs"),
            message_idempotency_key(&replay, "assistant", "check logs")
        );
        assert_ne!(
            message_idempotency_key(&serde_json::json!({ "event_id": "evt-1" }), "user", "ok"),
            message_idempotency_key(&serde_json::json!({ "event_id": "evt-2" }), "user", "ok")
        );
        // Without an explicit event id, two identical prompts must never collapse.
        let bare = serde_json::json!({ "session_id": "s1" });
        assert_ne!(
            message_idempotency_key(&bare, "user", "continue"),
            message_idempotency_key(&bare, "user", "continue")
        );
    }

    #[test]
    fn enqueue_and_failure_recording_keep_fifo_order_and_dedupe_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        assert!(enqueue_message(
            &path,
            "user",
            "Prompt 1".to_string(),
            "k1".to_string(),
            "2026-11-27T08:00:00Z".to_string(),
            Some("claude".to_string()),
        ));
        assert!(!enqueue_message(
            &path,
            "user",
            "Prompt 1".to_string(),
            "k1".to_string(),
            "2026-11-27T08:00:00Z".to_string(),
            Some("claude".to_string()),
        ));
        assert!(enqueue_message(
            &path,
            "assistant",
            "Reply 1".to_string(),
            "k2".to_string(),
            "2026-11-27T08:00:01Z".to_string(),
            None,
        ));

        record_delivery_failure(&path, "k1", "HTTP 503", false);
        let state = read_state_snapshot(&path);
        assert_eq!(state.pending.len(), 2);
        assert_eq!(state.pending[0].idempotency_key, "k1");
        assert_eq!(state.pending[0].attempts, 1);
        assert_eq!(state.pending[0].last_error.as_deref(), Some("HTTP 503"));
        assert_eq!(state.failures.len(), 1);

        mark_delivered(&path, "k1", Some(("tid-1".into(), "sid-1".into())));
        let state = read_state_snapshot(&path);
        assert_eq!(state.transcript_id.as_deref(), Some("tid-1"));
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].idempotency_key, "k2");
        assert!(!enqueue_message(
            &path,
            "user",
            "Prompt 1".to_string(),
            "k1".to_string(),
            "2026-11-27T08:00:00Z".to_string(),
            Some("claude".to_string()),
        ));
    }
}
