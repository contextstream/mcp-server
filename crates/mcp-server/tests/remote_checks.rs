//! Exercise the editor-hook fallback across real one-shot processes.
#![cfg(unix)]

use axum::{
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_opt_in_updates_hints_and_credit_refusals_run_locally_across_processes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let api = Router::new().route("/api/v1/mcp/hosted-tools/enabled", put(|Json(body): Json<Value>| async move {
        assert_eq!(body, json!({"enabled":true, "more_engines":true}));
        Json(json!({"tool":"check", "enabled":true, "available":true,"more_engines":true}))
    })).route("/api/v1/mcp/hosted-tools", get(|| async {
        Json(json!({"tools":[{"name":"check","description":"Remote checks","input_schema":{"type":"object"},
            "transport":{"kind":"stream"}, "attachments":["checkout_snapshot"],
            "command_hints":{"argument":"command","patterns":["^cargo check$"]}}]}))
    })).route("/api/v1/mcp/hosted-tools/check/call", post(move || async move {
        count.fetch_add(1, Ordering::SeqCst);
        (StatusCode::PAYMENT_REQUIRED, Json(json!({"error":{"code":"INSUFFICIENT_CREDITS","details":{"required":20,"available":3}}})))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success())
    };
    git(&["init", "-q"]);
    std::fs::write(repo.path().join("Gemfile"), "").unwrap();
    git(&["add", "Gemfile"]);
    git(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@example.test",
        "commit",
        "-qm",
        "initial",
    ]);

    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(args)
            .current_dir(repo.path())
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("CONTEXTSTREAM_API_URL", &url)
            .env("CONTEXTSTREAM_API_KEY", "stub-remote-checks")
            .env(
                "CONTEXTSTREAM_WORKSPACE_ID",
                "11111111-1111-4111-8111-111111111111",
            )
            .output()
            .unwrap()
    };
    let setup = cli(&["remote-checks", "on", "--more-engines", "--json"]);
    assert!(setup.status.success());
    let report: Value = serde_json::from_slice(&setup.stdout).unwrap();
    assert_eq!(report["project_languages"]["more"], json!(["Ruby"]));
    for expected_calls in [1, 1, 2] {
        if expected_calls == 2 {
            let marker = std::fs::read_dir(home.path().join(".contextstream"))
                .unwrap()
                .flatten()
                .find(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("remote-checks-credits-")
                })
                .unwrap();
            std::fs::File::open(marker.path())
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(181))
                .unwrap();
        }
        let run = cli(&[
            "run",
            "--tool",
            "check",
            "--",
            "printf local-output && exit 7",
        ]);
        assert_eq!(run.status.code(), Some(7));
        assert_eq!(run.stdout, b"local-output");
        assert!(String::from_utf8(run.stderr).unwrap().contains(
            "Running the command locally. Add credits at https://contextstream.io/account/billing"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_remote_checks_is_opt_in_and_a_refusal_never_breaks_setup() {
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let gets = reads.clone();
    let puts = writes.clone();
    let api = Router::new()
        .route("/api/v1/auth/me", get(|| async {
            Json(json!({"data":{"id":"11111111-1111-4111-8111-111111111111","email":"tester@example.test","created_at":"2026-01-01T00:00:00Z"}}))
        }))
        .route("/api/v1/mcp/hosted-tools/enabled", get(move || async move {
            gets.fetch_add(1,Ordering::SeqCst);
            Json(json!({"enabled":false,"more_engines":false,"available":true,"tool":"check"}))
        }).put(move |Json(body): Json<Value>| async move {
            puts.fetch_add(1,Ordering::SeqCst);
            assert_eq!(body,json!({"enabled":true,"more_engines":true}));
            StatusCode::FORBIDDEN
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Gemfile"), "").unwrap();
    for (extra, expected) in [
        (&[][..], (0, 0)),
        (&["--dry-run", "--remote-checks"], (1, 0)),
        (&["--remote-checks"], (2, 1)),
    ] {
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes"])
            .args(extra)
            .current_dir(repo.path())
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "")
            .env("NO_COLOR", "1")
            .env("CONTEXTSTREAM_API_URL", &url)
            .env("CONTEXTSTREAM_API_KEY", "stub-setup")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            (reads.load(Ordering::SeqCst), writes.load(Ordering::SeqCst)),
            expected
        );
        if expected.1 > 0 {
            assert!(String::from_utf8_lossy(&out.stdout)
                .contains("Remote checks are unavailable for this account."));
        }
    }
    server.abort();
}
