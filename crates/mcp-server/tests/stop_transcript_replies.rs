//! Regression tests for appending the final assistant reply to the session
//! transcript from the Stop hook.
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
    sync::{Arc, Mutex},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";
const TRANSCRIPT_ID: &str = "55555555-5555-4555-8555-555555555555";
const API_SESSION_ID: &str = "77777777-7777-4777-8777-777777777777";
const MAX_ASSISTANT_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    query: Option<String>,
    body: String,
}

#[derive(Clone, Debug)]
enum TranscriptBehavior {
    /// Return `TRANSCRIPT_ID` when `session_id` matches `expected_session_id` (or always if `None`).
    MatchSession {
        expected_session_id: Option<String>,
        append_status: StatusCode,
    },
    /// Return empty `items: []` on list.
    Empty,
    /// Return an HTTP error status on all `/api/v1/transcripts*` routes.
    Error(StatusCode),
}

async fn spawn_mock_api(
    behavior: TranscriptBehavior,
) -> (
    String,
    Arc<Mutex<Vec<CapturedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let cap_route = Arc::clone(&captured);

    let app = Router::new().fallback(any(move |method: Method, uri: Uri, body: Bytes| {
        let cap = Arc::clone(&cap_route);
        let behavior = behavior.clone();
        async move {
            let path = uri.path().to_string();
            let query = uri.query().map(str::to_string);
            cap.lock().unwrap().push(CapturedRequest {
                method: method.to_string(),
                path: path.clone(),
                query: query.clone(),
                body: String::from_utf8_lossy(&body).to_string(),
            });

            if path == "/api/v1/transcripts" && method == Method::GET {
                return match behavior {
                    TranscriptBehavior::MatchSession {
                        expected_session_id,
                        ..
                    } => {
                        let session_matches = match expected_session_id {
                            None => true,
                            Some(ref expected) => query
                                .as_deref()
                                .unwrap_or_default()
                                .split('&')
                                .any(|pair| pair == format!("session_id={expected}")),
                        };
                        let items = if session_matches {
                            vec![json!({
                                "id": TRANSCRIPT_ID,
                                "session_id": expected_session_id.unwrap_or_else(|| "sess".to_string()),
                            })]
                        } else {
                            Vec::new()
                        };
                        (
                            StatusCode::OK,
                            axum::Json(json!({
                                "success": true,
                                "data": {
                                    "items": items,
                                    "total": items.len(),
                                }
                            })),
                        )
                    }
                    TranscriptBehavior::Empty => (
                        StatusCode::OK,
                        axum::Json(json!({
                            "success": true,
                            "data": { "items": [], "total": 0 }
                        })),
                    ),
                    TranscriptBehavior::Error(status) => {
                        (status, axum::Json(json!({"error": "transcript api failure"})))
                    }
                };
            }

            if path.starts_with("/api/v1/transcripts/") && path.ends_with("/messages") {
                return match behavior {
                    TranscriptBehavior::MatchSession { append_status, .. } => (
                        append_status,
                        axum::Json(json!({"success": append_status.is_success()})),
                    ),
                    TranscriptBehavior::Error(status) => {
                        (status, axum::Json(json!({"error": "append failure"})))
                    }
                    TranscriptBehavior::Empty => (
                        StatusCode::NOT_FOUND,
                        axum::Json(json!({"error": "not found"})),
                    ),
                };
            }

            (StatusCode::OK, axum::Json(json!({"data": {"ok": true}})))
        }
    }));

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (api_url, captured, handle)
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
                "api_key": "cs_mock_stop_transcript_key",
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        cmd.args(["hook", hook_name])
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env_remove("CONTEXTSTREAM_API_KEY")
            .env_remove("CONTEXTSTREAM_API_URL")
            .env_remove("CONTEXTSTREAM_WORKSPACE_ID")
            .env_remove("CONTEXTSTREAM_PROJECT_ID")
            .env_remove("CONTEXTSTREAM_HOOK_ENABLED")
            .env_remove("CONTEXTSTREAM_STOP_ENABLED")
            .env_remove("CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
            .env_remove("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED")
            .env_remove("CONTEXTSTREAM_STOP_SAVE_TRANSCRIPT")
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
}

fn assert_empty_hook_output(out: &Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "{}",
        "{label} should emit empty JSON object"
    );
}

fn transcript_append_bodies(captured: &Mutex<Vec<CapturedRequest>>) -> Vec<Value> {
    captured
        .lock()
        .unwrap()
        .iter()
        .filter(|req| {
            req.method == "POST"
                && req.path == format!("/api/v1/transcripts/{TRANSCRIPT_ID}/messages")
        })
        .map(|req| serde_json::from_str::<Value>(&req.body).expect("valid append JSON"))
        .collect()
}

fn has_stop_checkpoint(captured: &Mutex<Vec<CapturedRequest>>) -> bool {
    captured.lock().unwrap().iter().any(|req| {
        req.method == "POST"
            && req.path == "/api/v1/memory/events"
            && serde_json::from_str::<Value>(&req.body)
                .ok()
                .and_then(|v| v.get("title").and_then(Value::as_str).map(str::to_string))
                .as_deref()
                == Some("Stop checkpoint")
    })
}

fn has_any_transcript_request(captured: &Mutex<Vec<CapturedRequest>>) -> bool {
    captured
        .lock()
        .unwrap()
        .iter()
        .any(|req| req.path.starts_with("/api/v1/transcripts"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_appends_assistant_reply_to_session_transcript_when_enabled() {
    let (api_url, captured, server_handle) = spawn_mock_api(TranscriptBehavior::MatchSession {
        expected_session_id: Some("sess-direct-1".to_string()),
        append_status: StatusCode::OK,
    })
    .await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    let out = sandbox.run_hook(
        "stop",
        &json!({
            "session_id": "sess-direct-1",
            "cwd": sandbox.repo,
            "reason": "end_turn",
            "last_assistant_message": "  I fixed the migration guard and ran tests.  ",
        }),
        &[],
    );
    assert_empty_hook_output(&out, "stop enabled");

    assert!(
        has_stop_checkpoint(&captured),
        "Stop checkpoint event must still be posted, got {:?}",
        *captured.lock().unwrap()
    );

    let lookup = captured
        .lock()
        .unwrap()
        .iter()
        .find(|req| req.method == "GET" && req.path == "/api/v1/transcripts")
        .cloned()
        .expect("expected GET /api/v1/transcripts lookup");
    let query = lookup.query.unwrap_or_default();
    assert!(
        query.contains("session_id=sess-direct-1"),
        "expected session_id query param, got {query}"
    );
    assert!(
        query.contains(&format!("workspace_id={WORKSPACE_ID}")),
        "expected workspace_id query param, got {query}"
    );
    assert!(
        query.contains(&format!("project_id={PROJECT_ID}")),
        "expected project_id query param, got {query}"
    );

    let appended = transcript_append_bodies(&captured);
    assert_eq!(
        appended.len(),
        1,
        "expected one assistant message append, got {:?}",
        *captured.lock().unwrap()
    );
    assert_eq!(appended[0]["role"], "assistant");
    assert_eq!(
        appended[0]["content"],
        "I fixed the migration guard and ran tests."
    );
    assert!(
        appended[0]["timestamp"]
            .as_str()
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .is_some(),
        "expected RFC3339 timestamp, got {:?}",
        appended[0]
    );

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_uses_init_api_session_id_for_transcript_lookup() {
    let (api_url, captured, server_handle) = spawn_mock_api(TranscriptBehavior::MatchSession {
        expected_session_id: Some(API_SESSION_ID.to_string()),
        append_status: StatusCode::OK,
    })
    .await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    // Simulate PostToolUse observing `init` returning ContextStream's API session_id.
    let init_out = sandbox.run_hook(
        "post-tool-use",
        &json!({
            "hook_event_name": "PostToolUse",
            "session_id": "host-claude-42",
            "cwd": sandbox.repo,
            "tool_name": "mcp__contextstream__init",
            "tool_input": {"folder_path": sandbox.repo},
            "tool_response": {
                "isError": false,
                "content": [{"type": "text", "text": "Session ready"}],
                "structuredContent": {
                    "session_id": API_SESSION_ID,
                    "workspace_id": WORKSPACE_ID,
                    "project_id": PROJECT_ID,
                }
            }
        }),
        &[],
    );
    assert!(
        init_out.status.success(),
        "post-tool-use init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    captured.lock().unwrap().clear();

    let stop_out = sandbox.run_hook(
        "stop",
        &json!({
            "session_id": "host-claude-42",
            "cwd": sandbox.repo,
            "last_assistant_message": "Deployed the fix and verified metrics.",
        }),
        &[],
    );
    assert_empty_hook_output(&stop_out, "stop with init api session_id");

    let appended = transcript_append_bodies(&captured);
    assert_eq!(
        appended.len(),
        1,
        "expected append using API session_id {API_SESSION_ID}, got {:?}",
        *captured.lock().unwrap()
    );
    assert_eq!(appended[0]["role"], "assistant");
    assert_eq!(
        appended[0]["content"],
        "Deployed the fix and verified metrics."
    );

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_skips_transcript_when_disabled() {
    let (api_url, captured, server_handle) = spawn_mock_api(TranscriptBehavior::MatchSession {
        expected_session_id: None,
        append_status: StatusCode::OK,
    })
    .await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    let base_payload = json!({
        "session_id": "sess-disabled-1",
        "cwd": sandbox.repo,
        "last_assistant_message": "CONFIDENTIAL_ASSISTANT_REPLY_SHOULD_NOT_APPEND",
    });

    // 1. Environment variable opt-outs.
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
            "CONTEXTSTREAM_STOP_SAVE_TRANSCRIPT=false",
            vec![("CONTEXTSTREAM_STOP_SAVE_TRANSCRIPT", "false")],
        ),
        (
            "fail-closed when transcripts=false and hook_transcripts=true",
            vec![
                ("CONTEXTSTREAM_TRANSCRIPTS_ENABLED", "false"),
                ("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED", "true"),
            ],
        ),
    ] {
        captured.lock().unwrap().clear();
        let out = sandbox.run_hook("stop", &base_payload, &extra_env);
        assert_empty_hook_output(&out, label);
        assert!(
            !has_any_transcript_request(&captured),
            "{label}: must not call /api/v1/transcripts, got {:?}",
            *captured.lock().unwrap()
        );
        assert!(
            has_stop_checkpoint(&captured),
            "{label}: Stop checkpoint event must still be posted"
        );
    }

    // 2. Hook payload overrides.
    for key in ["transcripts_enabled", "save_transcript"] {
        captured.lock().unwrap().clear();
        let mut payload = base_payload.clone();
        payload[key] = json!(false);
        let out = sandbox.run_hook("stop", &payload, &[]);
        assert_empty_hook_output(&out, key);
        assert!(
            !has_any_transcript_request(&captured),
            "payload {key}=false must not call /api/v1/transcripts, got {:?}",
            *captured.lock().unwrap()
        );
        assert!(has_stop_checkpoint(&captured));
    }

    // 3. Hosted MCP header opt-outs.
    for (transcripts, hook_transcripts) in [("false", "true"), ("true", "false")] {
        sandbox.write_hosted_mcp_headers(transcripts, hook_transcripts);
        captured.lock().unwrap().clear();
        let out = sandbox.run_hook("stop", &base_payload, &[]);
        assert_empty_hook_output(&out, "hosted headers opt-out");
        assert!(
            !has_any_transcript_request(&captured),
            "headers ({transcripts}, {hook_transcripts}) must not call /api/v1/transcripts, got {:?}",
            *captured.lock().unwrap()
        );
        assert!(has_stop_checkpoint(&captured));
    }

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_caps_long_assistant_reply_on_utf8_boundary() {
    let (api_url, captured, server_handle) = spawn_mock_api(TranscriptBehavior::MatchSession {
        expected_session_id: Some("sess-cap-1".to_string()),
        append_status: StatusCode::OK,
    })
    .await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    // 10,000 copies of 2-byte 'é' = 20,000 bytes (> 16 KiB cap), guaranteed to
    // straddle a multi-byte character boundary at odd byte offsets.
    let long_reply = "\u{E9}".repeat(10_000);
    let out = sandbox.run_hook(
        "stop",
        &json!({
            "session_id": "sess-cap-1",
            "cwd": sandbox.repo,
            "last_assistant_message": long_reply,
        }),
        &[],
    );
    assert_empty_hook_output(&out, "stop long reply");

    let appended = transcript_append_bodies(&captured);
    assert_eq!(appended.len(), 1, "expected capped assistant append");
    assert_eq!(appended[0]["role"], "assistant");
    let content = appended[0]["content"].as_str().expect("string content");
    assert!(
        content.len() <= MAX_ASSISTANT_MESSAGE_BYTES,
        "content length {} exceeded cap {}",
        content.len(),
        MAX_ASSISTANT_MESSAGE_BYTES
    );
    let prefix = content
        .strip_suffix("...")
        .expect("truncated content should end with ellipsis");
    assert!(!prefix.is_empty());
    assert!(prefix.chars().all(|ch| ch == '\u{E9}'));

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_scrubs_credentials_and_setup_secrets() {
    let (api_url, captured, server_handle) = spawn_mock_api(TranscriptBehavior::MatchSession {
        expected_session_id: Some("sess-scrub-1".to_string()),
        append_status: StatusCode::OK,
    })
    .await;
    let sandbox = Sandbox::new(&api_url);
    sandbox.write_hosted_mcp_headers("true", "true");

    // Normal session: credential tokens redacted, regular 6-digit numbers kept.
    let out = sandbox.run_hook(
        "stop",
        &json!({
            "session_id": "sess-scrub-1",
            "cwd": sandbox.repo,
            "last_assistant_message": "Key cbiq_secret123_abc and token cbst_tok_999 for issue 123456.",
        }),
        &[],
    );
    assert_empty_hook_output(&out, "stop normal scrub");
    let appended = transcript_append_bodies(&captured);
    assert_eq!(appended.len(), 1);
    let content = appended[0]["content"].as_str().unwrap();
    assert!(!content.contains("cbiq_secret123_abc"), "{content}");
    assert!(!content.contains("cbst_tok_999"), "{content}");
    assert_eq!(
        content,
        "Key [redacted-credential] and token [redacted-credential] for issue 123456."
    );

    // Account-setup session: also scrub 6-digit verification codes, phone numbers, and device codes.
    let transcript_path: PathBuf = sandbox.repo.join("setup-transcript.jsonl");
    std::fs::write(
        &transcript_path,
        format!(
            "{}\n",
            json!({"type": "tool_use", "name": "mcp__contextstream__account"})
        ),
    )
    .unwrap();
    captured.lock().unwrap().clear();

    let out = sandbox.run_hook(
        "stop",
        &json!({
            "session_id": "sess-scrub-1",
            "cwd": sandbox.repo,
            "transcript_path": Path::new(&transcript_path),
            "last_assistant_message": "Code 483920 at +1 (555) 010-1234 device ABCD-EFGH with cbiq_secret_xyz.",
        }),
        &[],
    );
    assert_empty_hook_output(&out, "stop account-setup scrub");
    let appended = transcript_append_bodies(&captured);
    assert_eq!(appended.len(), 1);
    let content = appended[0]["content"].as_str().unwrap();
    for secret in [
        "483920",
        "+1 (555) 010-1234",
        "ABCD-EFGH",
        "cbiq_secret_xyz",
    ] {
        assert!(!content.contains(secret), "leaked {secret} in {content}");
    }
    assert!(content.contains("[code]"), "{content}");
    assert!(content.contains("[phone]"), "{content}");
    assert!(content.contains("[device-code]"), "{content}");
    assert!(content.contains("[redacted-credential]"), "{content}");

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_ignores_failed_or_missing_transcript_and_keeps_checkpoint() {
    // 1. Transcript API returns 500 -> hook still exits 0 with `{}` and posts Stop checkpoint.
    let (err_url, err_captured, err_handle) =
        spawn_mock_api(TranscriptBehavior::Error(StatusCode::INTERNAL_SERVER_ERROR)).await;
    let err_sandbox = Sandbox::new(&err_url);
    let out = err_sandbox.run_hook(
        "stop",
        &json!({
            "session_id": "sess-err-1",
            "cwd": err_sandbox.repo,
            "last_assistant_message": "Done.",
        }),
        &[],
    );
    assert_empty_hook_output(&out, "stop transcript 500");
    assert!(has_stop_checkpoint(&err_captured));
    err_handle.abort();

    // 2. No transcript exists for this session -> no append request, hook still succeeds.
    let (empty_url, empty_captured, empty_handle) = spawn_mock_api(TranscriptBehavior::Empty).await;
    let empty_sandbox = Sandbox::new(&empty_url);
    let out = empty_sandbox.run_hook(
        "stop",
        &json!({
            "session_id": "sess-none-1",
            "cwd": empty_sandbox.repo,
            "last_assistant_message": "Done.",
        }),
        &[],
    );
    assert_empty_hook_output(&out, "stop no transcript");
    assert!(transcript_append_bodies(&empty_captured).is_empty());
    assert!(has_stop_checkpoint(&empty_captured));

    // 3. Blank assistant message -> zero transcript requests.
    empty_captured.lock().unwrap().clear();
    let out = empty_sandbox.run_hook(
        "stop",
        &json!({
            "session_id": "sess-none-1",
            "cwd": empty_sandbox.repo,
            "last_assistant_message": "   \n  ",
        }),
        &[],
    );
    assert_empty_hook_output(&out, "stop blank reply");
    assert!(!has_any_transcript_request(&empty_captured));
    assert!(has_stop_checkpoint(&empty_captured));

    empty_handle.abort();
}
