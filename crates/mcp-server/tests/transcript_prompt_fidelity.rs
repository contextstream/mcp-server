//! Regression tests for session transcript prompt fidelity (ticket 1a81456d):
//! 1. Every `UserPromptSubmit` lands in the transcript once, in order, verbatim.
//! 2. Prompt and Stop reply appends racing each other preserve both without drops
//!    or duplicate transcripts.
//! 3. Short follow-up prompts are stored verbatim without parenthetical context.
//! 4. Transient failures/timeouts retry or record the failure and drain in order.
//! 5. Fail-closed privacy controls, 16 KiB UTF-8 cap, and secret scrubbing hold.
#![cfg(unix)]

use axum::{
    body::Bytes,
    http::{Method, StatusCode, Uri},
    routing::any,
    Router,
};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";
const API_SESSION_ID: &str = "77777777-7777-4777-8777-777777777777";
const MAX_TRANSCRIPT_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
struct StoredTranscript {
    id: String,
    session_id: String,
    messages: Vec<Value>,
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
}

#[derive(Default)]
struct MockState {
    transcripts: Vec<StoredTranscript>,
    requests: Vec<CapturedRequest>,
    next_id: usize,
    /// Delay applied before responding to `/api/v1/transcripts*` requests.
    transcript_delay: Duration,
    /// When true, all `/api/v1/transcripts*` calls return 500.
    fail_all_transcripts: bool,
    /// Number of upcoming `POST /api/v1/transcripts/*/messages` calls to fail with 503.
    fail_next_appends: AtomicUsize,
}

async fn spawn_stateful_mock_api() -> (String, Arc<Mutex<MockState>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(MockState::default()));
    let route_state = Arc::clone(&state);

    let app = Router::new().fallback(any(move |method: Method, uri: Uri, body: Bytes| {
        let state = Arc::clone(&route_state);
        async move {
            let path = uri.path().to_string();
            let query = uri.query().map(str::to_string);
            let body_str = String::from_utf8_lossy(&body).to_string();

            let delay = {
                let mut guard = state.lock().unwrap();
                guard.requests.push(CapturedRequest { path: path.clone() });
                if path.starts_with("/api/v1/transcripts") {
                    guard.transcript_delay
                } else {
                    Duration::ZERO
                }
            };

            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }

            if path == "/api/v1/context/hook" && method == Method::POST {
                return (
                    StatusCode::OK,
                    axum::Json(json!({
                        "success": true,
                        "data": {
                            "context": "[HOOK CONTEXT] Ready."
                        }
                    })),
                );
            }

            if path.starts_with("/api/v1/transcripts") {
                let mut guard = state.lock().unwrap();
                if guard.fail_all_transcripts {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({ "error": "injected transcript outage" })),
                    );
                }

                if path == "/api/v1/transcripts" && method == Method::GET {
                    let wanted_session = query.as_deref().and_then(|q| {
                        q.split('&')
                            .find_map(|pair| pair.strip_prefix("session_id="))
                    });
                    let items: Vec<Value> = guard
                        .transcripts
                        .iter()
                        .filter(|t| wanted_session.is_none_or(|s| t.session_id == s))
                        .map(|t| {
                            json!({
                                "id": t.id,
                                "session_id": t.session_id,
                                "messages": t.messages,
                            })
                        })
                        .collect();
                    return (
                        StatusCode::OK,
                        axum::Json(json!({
                            "success": true,
                            "data": {
                                "items": items,
                                "total": items.len(),
                            }
                        })),
                    );
                }

                if path == "/api/v1/transcripts" && method == Method::POST {
                    let payload: Value = serde_json::from_str(&body_str).unwrap_or_default();
                    guard.next_id += 1;
                    let id = format!("55555555-5555-4555-8555-{:012}", guard.next_id);
                    let session_id = payload
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let messages = payload
                        .get("messages")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    guard.transcripts.push(StoredTranscript {
                        id: id.clone(),
                        session_id: session_id.clone(),
                        messages,
                    });
                    return (
                        StatusCode::CREATED,
                        axum::Json(json!({
                            "success": true,
                            "data": {
                                "id": id,
                                "session_id": session_id,
                            }
                        })),
                    );
                }

                if let Some(rest) = path.strip_prefix("/api/v1/transcripts/") {
                    if let Some(tid) = rest.strip_suffix("/messages") {
                        if method == Method::POST {
                            let prev = guard.fail_next_appends.fetch_update(
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                                |n| n.checked_sub(1),
                            );
                            if prev.is_ok() {
                                return (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    axum::Json(json!({ "error": "transient 503" })),
                                );
                            }

                            let payload: Value =
                                serde_json::from_str(&body_str).unwrap_or_default();
                            if let Some(transcript) =
                                guard.transcripts.iter_mut().find(|t| t.id == tid)
                            {
                                transcript.messages.push(payload);
                                return (
                                    StatusCode::OK,
                                    axum::Json(json!({
                                        "success": true,
                                        "data": { "id": tid }
                                    })),
                                );
                            }
                            return (
                                StatusCode::NOT_FOUND,
                                axum::Json(json!({ "error": "transcript not found" })),
                            );
                        }
                    }
                }
            }

            (
                StatusCode::OK,
                axum::Json(json!({ "data": { "ok": true } })),
            )
        }
    }));

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (api_url, state, handle)
}

struct Sandbox {
    _temp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Sandbox {
    fn new(api_url: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(home.join(".contextstream")).unwrap();
        std::fs::create_dir_all(repo.join(".contextstream")).unwrap();

        std::fs::write(
            home.join(".contextstream").join("credentials.json"),
            serde_json::to_string_pretty(&json!({
                "api_key": "cs_mock_transcript_prompt_key",
                "api_url": api_url,
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            repo.join(".contextstream").join("config.json"),
            serde_json::to_string_pretty(&json!({
                "workspace_id": WORKSPACE_ID,
                "project_id": PROJECT_ID,
            }))
            .unwrap(),
        )
        .unwrap();

        Self {
            _temp: temp,
            home,
            repo,
        }
    }

    fn write_hosted_mcp_headers(&self, transcripts: &str, hook_transcripts: &str) {
        let entry = json!({
            "mcpServers": {
                "contextstream": {
                    "type": "http",
                    "url": "https://mcp.contextstream.io/mcp?default_context_mode=fast",
                    "headers": {
                        "X-ContextStream-Client": "claude-code",
                        "X-ContextStream-Workspace-Id": WORKSPACE_ID,
                        "X-ContextStream-Project-Id": PROJECT_ID,
                        "X-ContextStream-Transcripts-Enabled": transcripts,
                        "X-ContextStream-Hook-Transcripts-Enabled": hook_transcripts,
                    }
                }
            }
        });
        std::fs::write(
            self.repo.join(".mcp.json"),
            serde_json::to_string_pretty(&entry).unwrap(),
        )
        .unwrap();
    }

    fn run_hook(&self, hook_name: &str, payload: &Value, extra_env: &[(&str, &str)]) -> Output {
        run_hook_in(&self.home, &self.repo, hook_name, payload, extra_env)
    }
}

fn run_hook_in(
    home: &Path,
    repo: &Path,
    hook_name: &str,
    payload: &Value,
    extra_env: &[(&str, &str)],
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    cmd.args(["hook", hook_name])
        .current_dir(repo)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("CONTEXTSTREAM_API_KEY")
        .env_remove("CONTEXTSTREAM_API_URL")
        .env_remove("CONTEXTSTREAM_WORKSPACE_ID")
        .env_remove("CONTEXTSTREAM_PROJECT_ID")
        .env_remove("CONTEXTSTREAM_HOOK_ENABLED")
        .env_remove("CONTEXTSTREAM_REMINDER_ENABLED")
        .env_remove("CONTEXTSTREAM_STOP_ENABLED")
        .env_remove("CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
        .env_remove("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED")
        .env_remove("CONTEXTSTREAM_STOP_SAVE_TRANSCRIPT")
        .env_remove("CONTEXTSTREAM_USER_PROMPT_SAVE_TRANSCRIPT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn hook");
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(serde_json::to_string(payload).unwrap().as_bytes())
            .expect("write stdin");
    }
    child.wait_with_output().expect("wait hook")
}

fn assert_hook_ok(out: &Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_prompt_session_keeps_three_user_messages_in_order() {
    let (api_url, state, server_handle) = spawn_stateful_mock_api().await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    let host_session = "host-three-prompts";
    let prompts = [
        "Open a sev2: balance queue backed up in payments-worker.",
        "Update: the queue worker has about 1,900 balance requests waiting, so we added four consumers.",
        "Update: the backlog is gone and latency is back to normal.",
    ];
    let replies = [
        "Opened sev2 incident for the balance queue backlog.",
        "Recorded the consumer scale-up on the incident.",
        "Marked the incident mitigated.",
    ];

    // Turn 1: prompt -> init observed by PostToolUse -> stop reply.
    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "prompt": prompts[0],
        }),
        &[],
    );
    assert_hook_ok(&out, "prompt 1");

    let init_out = sandbox.run_hook(
        "post-tool-use",
        &json!({
            "hook_event_name": "PostToolUse",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "tool_name": "mcp__contextstream__init",
            "tool_input": { "folder_path": sandbox.repo },
            "tool_response": {
                "isError": false,
                "content": [{ "type": "text", "text": "Session ready" }],
                "structuredContent": {
                    "session_id": API_SESSION_ID,
                    "workspace_id": WORKSPACE_ID,
                    "project_id": PROJECT_ID,
                }
            }
        }),
        &[],
    );
    assert_hook_ok(&init_out, "post-tool-use init");

    let out = sandbox.run_hook(
        "stop",
        &json!({
            "hook_event_name": "Stop",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "last_assistant_message": replies[0],
        }),
        &[],
    );
    assert_hook_ok(&out, "stop 1");

    // Turns 2 & 3: prompt -> stop reply (even without a `context()` tool call).
    for idx in [1, 2] {
        let out = sandbox.run_hook(
            "user-prompt-submit",
            &json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": host_session,
                "cwd": sandbox.repo,
                "prompt": prompts[idx],
            }),
            &[],
        );
        assert_hook_ok(&out, &format!("prompt {}", idx + 1));

        let out = sandbox.run_hook(
            "stop",
            &json!({
                "hook_event_name": "Stop",
                "session_id": host_session,
                "cwd": sandbox.repo,
                "last_assistant_message": replies[idx],
            }),
            &[],
        );
        assert_hook_ok(&out, &format!("stop {}", idx + 1));
    }

    let transcripts = state.lock().unwrap().transcripts.clone();
    assert_eq!(
        transcripts.len(),
        1,
        "session must stay in a single transcript, got {transcripts:?}"
    );
    let messages = &transcripts[0].messages;
    let role_and_text: Vec<(&str, &str)> = messages
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or(""),
                m["content"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        role_and_text,
        vec![
            ("user", prompts[0]),
            ("assistant", replies[0]),
            ("user", prompts[1]),
            ("assistant", replies[1]),
            ("user", prompts[2]),
            ("assistant", replies[2]),
        ],
        "expected all three prompts and replies in order"
    );

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_and_reply_appends_racing_each_other_keep_both() {
    let (api_url, state, server_handle) = spawn_stateful_mock_api().await;
    state.lock().unwrap().transcript_delay = Duration::from_millis(60);
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    let host_session = "host-race-session";
    let home_a = sandbox.home.clone();
    let repo_a = sandbox.repo.clone();
    let home_b = sandbox.home.clone();
    let repo_b = sandbox.repo.clone();

    // Turn 1 cold race: UserPromptSubmit and Stop start concurrently before
    // any transcript exists on the server.
    let prompt_handle = std::thread::spawn(move || {
        run_hook_in(
            &home_a,
            &repo_a,
            "user-prompt-submit",
            &json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": host_session,
                "cwd": repo_a,
                "prompt": "First prompt in race.",
            }),
            &[],
        )
    });
    let stop_handle = std::thread::spawn(move || {
        run_hook_in(
            &home_b,
            &repo_b,
            "stop",
            &json!({
                "hook_event_name": "Stop",
                "session_id": host_session,
                "cwd": repo_b,
                "last_assistant_message": "First reply in race.",
            }),
            &[],
        )
    });
    assert_hook_ok(&prompt_handle.join().unwrap(), "cold race prompt");
    assert_hook_ok(&stop_handle.join().unwrap(), "cold race stop");

    // Turn 2 warm race: transcript already exists; prompt and reply race again.
    let home_c = sandbox.home.clone();
    let repo_c = sandbox.repo.clone();
    let home_d = sandbox.home.clone();
    let repo_d = sandbox.repo.clone();
    let prompt2 = std::thread::spawn(move || {
        run_hook_in(
            &home_c,
            &repo_c,
            "user-prompt-submit",
            &json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": host_session,
                "cwd": repo_c,
                "prompt": "Second prompt in race.",
            }),
            &[],
        )
    });
    let stop2 = std::thread::spawn(move || {
        run_hook_in(
            &home_d,
            &repo_d,
            "stop",
            &json!({
                "hook_event_name": "Stop",
                "session_id": host_session,
                "cwd": repo_d,
                "last_assistant_message": "Second reply in race.",
            }),
            &[],
        )
    });
    assert_hook_ok(&prompt2.join().unwrap(), "warm race prompt");
    assert_hook_ok(&stop2.join().unwrap(), "warm race stop");

    let transcripts = state.lock().unwrap().transcripts.clone();
    assert_eq!(
        transcripts.len(),
        1,
        "racing hooks must not split into duplicate transcripts, got {transcripts:?}"
    );
    let contents: Vec<(&str, &str)> = transcripts[0]
        .messages
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or(""),
                m["content"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        contents.len(),
        4,
        "both prompts and both replies must survive concurrent races: {contents:?}"
    );
    assert!(contents.contains(&("user", "First prompt in race.")));
    assert!(contents.contains(&("assistant", "First reply in race.")));
    assert!(contents.contains(&("user", "Second prompt in race.")));
    assert!(contents.contains(&("assistant", "Second reply in race.")));
    // Cold turn 1 must always create the transcript with the user prompt first.
    assert_eq!(contents[0], ("user", "First prompt in race."));

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_follow_up_prompt_is_stored_verbatim() {
    let (api_url, state, server_handle) = spawn_stateful_mock_api().await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    let host_session = "host-followup-verbatim";
    let local_transcript = sandbox.repo.join("claude-session.jsonl");
    std::fs::write(
        &local_transcript,
        format!(
            "{}\n{}\n",
            json!({
                "type": "user",
                "message": { "role": "user", "content": "Index the 504 screenshot from checkout." }
            }),
            json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": "Indexed the 504 screenshot." }
            }),
        ),
    )
    .unwrap();

    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "transcript_path": local_transcript,
            "prompt": "Index the 504 screenshot from checkout.",
        }),
        &[],
    );
    assert_hook_ok(&out, "prompt 1");

    let out = sandbox.run_hook(
        "stop",
        &json!({
            "hook_event_name": "Stop",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "transcript_path": local_transcript,
            "last_assistant_message": "Indexed the 504 screenshot.",
        }),
        &[],
    );
    assert_hook_ok(&out, "stop 1");

    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "transcript_path": local_transcript,
            "prompt": "What does it tell us?",
        }),
        &[],
    );
    assert_hook_ok(&out, "short follow-up prompt");

    let transcripts = state.lock().unwrap().transcripts.clone();
    assert_eq!(transcripts.len(), 1);
    let user_messages: Vec<&str> = transcripts[0]
        .messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(
        user_messages,
        vec![
            "Index the 504 screenshot from checkout.",
            "What does it tell us?",
        ],
        "short follow-up prompt must be stored verbatim without parenthetical additions"
    );

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_prompt_write_retries_and_recovers_in_order_without_silent_drop() {
    let (api_url, state, server_handle) = spawn_stateful_mock_api().await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    let host_session = "host-retry-session";

    // Turn 1 succeeds and establishes the transcript.
    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "prompt": "Prompt one.",
        }),
        &[],
    );
    assert_hook_ok(&out, "prompt 1");

    // Turn 2: first append hits transient 503 -> in-hook retry succeeds.
    state
        .lock()
        .unwrap()
        .fail_next_appends
        .store(1, Ordering::SeqCst);
    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "prompt": "Prompt two (transient 503).",
        }),
        &[],
    );
    assert_hook_ok(&out, "prompt 2 transient retry");

    {
        let transcripts = &state.lock().unwrap().transcripts;
        assert_eq!(transcripts.len(), 1);
        let contents: Vec<&str> = transcripts[0]
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(contents, vec!["Prompt one.", "Prompt two (transient 503)."]);
    }

    // Turn 3: complete outage during UserPromptSubmit -> failure recorded,
    // prompt stays queued, and Stop flushes Prompt 3 BEFORE Reply 3 once the
    // API recovers.
    state.lock().unwrap().fail_all_transcripts = true;
    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "prompt": "Prompt three (outage then recovery).",
        }),
        &[],
    );
    assert_hook_ok(&out, "prompt 3 during outage");
    assert!(
        !String::from_utf8_lossy(&out.stderr).trim().is_empty(),
        "failed transcript append must record diagnostic rather than dropping silently"
    );

    state.lock().unwrap().fail_all_transcripts = false;
    let out = sandbox.run_hook(
        "stop",
        &json!({
            "hook_event_name": "Stop",
            "session_id": host_session,
            "cwd": sandbox.repo,
            "last_assistant_message": "Reply three after recovery.",
        }),
        &[],
    );
    assert_hook_ok(&out, "stop 3 recovery drain");

    let transcripts = state.lock().unwrap().transcripts.clone();
    assert_eq!(transcripts.len(), 1);
    let pairs: Vec<(&str, &str)> = transcripts[0]
        .messages
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or(""),
                m["content"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("user", "Prompt one."),
            ("user", "Prompt two (transient 503)."),
            ("user", "Prompt three (outage then recovery)."),
            ("assistant", "Reply three after recovery."),
        ],
        "queued prompt from outage must drain before Turn 3's assistant reply"
    );

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotency_dedupes_replayed_event_without_collapsing_distinct_prompts() {
    let (api_url, state, server_handle) = spawn_stateful_mock_api().await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    let host_session = "host-idempotency-session";

    // Two distinct turns with identical short text ("continue") must BOTH land.
    for turn in 1..=2 {
        let out = sandbox.run_hook(
            "user-prompt-submit",
            &json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": host_session,
                "cwd": sandbox.repo,
                "prompt": "continue",
            }),
            &[],
        );
        assert_hook_ok(&out, &format!("identical prompt turn {turn}"));
    }

    // Replaying the exact same event_id must deduplicate to a single append.
    for attempt in 1..=2 {
        let out = sandbox.run_hook(
            "user-prompt-submit",
            &json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": host_session,
                "event_id": "evt-replay-1",
                "cwd": sandbox.repo,
                "prompt": "check logs",
            }),
            &[],
        );
        assert_hook_ok(&out, &format!("replayed event attempt {attempt}"));
    }

    // A new event_id with the same text is a distinct turn and must land.
    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": host_session,
            "event_id": "evt-replay-2",
            "cwd": sandbox.repo,
            "prompt": "check logs",
        }),
        &[],
    );
    assert_hook_ok(&out, "new event_id same text");

    let transcripts = state.lock().unwrap().transcripts.clone();
    assert_eq!(transcripts.len(), 1);
    let user_texts: Vec<&str> = transcripts[0]
        .messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(
        user_texts,
        vec!["continue", "continue", "check logs", "check logs"]
    );

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_transcript_capture_preserves_fail_closed_cap_and_scrubbing() {
    let (api_url, state, server_handle) = spawn_stateful_mock_api().await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    // 1. Fail-closed opt-outs must make zero /api/v1/transcripts* calls.
    let opt_out_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "host-opt-out",
        "cwd": sandbox.repo,
        "prompt": "CONFIDENTIAL_PROMPT_SHOULD_NOT_APPEND",
    });

    for (label, extra_env) in [
        (
            "CONTEXTSTREAM_TRANSCRIPTS_ENABLED=false",
            vec![("CONTEXTSTREAM_TRANSCRIPTS_ENABLED", "false")],
        ),
        (
            "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED=false",
            vec![("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED", "false")],
        ),
        (
            "CONTEXTSTREAM_USER_PROMPT_SAVE_TRANSCRIPT=false",
            vec![("CONTEXTSTREAM_USER_PROMPT_SAVE_TRANSCRIPT", "false")],
        ),
        (
            "fail-closed mixed env",
            vec![
                ("CONTEXTSTREAM_TRANSCRIPTS_ENABLED", "false"),
                ("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED", "true"),
            ],
        ),
    ] {
        state.lock().unwrap().requests.clear();
        let out = sandbox.run_hook("user-prompt-submit", &opt_out_payload, &extra_env);
        assert_hook_ok(&out, label);
        let has_transcript_call = state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|r| r.path.starts_with("/api/v1/transcripts"));
        assert!(!has_transcript_call, "{label}: must not touch /transcripts");
    }

    for key in ["transcripts_enabled", "save_transcript"] {
        state.lock().unwrap().requests.clear();
        let mut payload = opt_out_payload.clone();
        payload[key] = json!(false);
        let out = sandbox.run_hook("user-prompt-submit", &payload, &[]);
        assert_hook_ok(&out, key);
        let has_transcript_call = state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|r| r.path.starts_with("/api/v1/transcripts"));
        assert!(!has_transcript_call, "payload {key}=false must opt out");
    }

    for (t, ht) in [("false", "true"), ("true", "false")] {
        sandbox.write_hosted_mcp_headers(t, ht);
        state.lock().unwrap().requests.clear();
        let out = sandbox.run_hook("user-prompt-submit", &opt_out_payload, &[]);
        assert_hook_ok(&out, "hosted headers opt-out");
        let has_transcript_call = state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|r| r.path.starts_with("/api/v1/transcripts"));
        assert!(!has_transcript_call, "headers ({t}, {ht}) must fail closed");
    }

    // Re-enable for cap + scrubbing checks.
    sandbox.write_hosted_mcp_headers("true", "true");

    // 2. Harness notifications must not be recorded as user prompts.
    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "host-harness-notif",
            "cwd": sandbox.repo,
            "prompt": "<task-notification><status>completed</status></task-notification>",
        }),
        &[],
    );
    assert_hook_ok(&out, "harness notification");
    assert!(
        state.lock().unwrap().transcripts.is_empty(),
        "harness notifications must not create transcripts"
    );

    // 3. 16 KiB UTF-8 cap + credential scrubbing + setup scrubbing.
    let long_prompt = "\u{E9}".repeat(10_000);
    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "host-cap-and-scrub",
            "cwd": sandbox.repo,
            "prompt": long_prompt,
        }),
        &[],
    );
    assert_hook_ok(&out, "16 KiB cap prompt");

    let setup_transcript = sandbox.repo.join("setup.jsonl");
    std::fs::write(
        &setup_transcript,
        format!(
            "{}\n",
            json!({"type": "tool_use", "name": "mcp__contextstream__account"})
        ),
    )
    .unwrap();
    let out = sandbox.run_hook(
        "user-prompt-submit",
        &json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "host-cap-and-scrub",
            "cwd": sandbox.repo,
            "transcript_path": setup_transcript,
            "prompt": "Code 483920 at +1 (555) 010-1234 device ABCD-EFGH with cbiq_secret_xyz.",
        }),
        &[],
    );
    assert_hook_ok(&out, "setup scrub prompt");

    let transcripts = state.lock().unwrap().transcripts.clone();
    assert_eq!(transcripts.len(), 1);
    assert_eq!(transcripts[0].messages.len(), 2);

    let capped = transcripts[0].messages[0]["content"].as_str().unwrap();
    assert!(capped.len() <= MAX_TRANSCRIPT_MESSAGE_BYTES);
    let prefix = capped.strip_suffix("...").expect("ellipsis on truncation");
    assert!(!prefix.is_empty());
    assert!(prefix.chars().all(|ch| ch == '\u{E9}'));

    let scrubbed = transcripts[0].messages[1]["content"].as_str().unwrap();
    for secret in [
        "483920",
        "+1 (555) 010-1234",
        "ABCD-EFGH",
        "cbiq_secret_xyz",
    ] {
        assert!(!scrubbed.contains(secret), "leaked {secret} in {scrubbed}");
    }
    assert!(scrubbed.contains("[code]"));
    assert!(scrubbed.contains("[phone]"));
    assert!(scrubbed.contains("[device-code]"));
    assert!(scrubbed.contains("[redacted-credential]"));

    server_handle.abort();
}
