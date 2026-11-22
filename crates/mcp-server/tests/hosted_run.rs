//! Run the actual CLI in separate processes against a local catalog and worker.
#![cfg(unix)]

use axum::{
    http::{HeaderMap, StatusCode, Uri},
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use mcp_client::command_hints::{self, Cache};
use mcp_client::frames::{encode_frame, FrameBuffer};
use serde_json::{json, Value};
use std::process::Output;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

const KEY: &str = "stub-hosted-run";
const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const CATALOG: &str = "/api/v1/mcp/hosted-tools";
const CALL: &str = "/api/v1/mcp/hosted-tools/check/call";
const CALL_ID: &str = "22222222-2222-4222-8222-222222222222";
const COMMAND: &str = "printf local-output && exit 7";

struct Cli {
    url: String,
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
}

impl Cli {
    fn new(url: String) -> Self {
        let cli = Self {
            url,
            home: tempfile::tempdir().unwrap(),
            repo: tempfile::tempdir().unwrap(),
        };
        for args in [
            &["init", "-q"][..],
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "--allow-empty",
                "--no-gpg-sign",
                "-qm",
                "initial",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(cli.repo.path())
                .env("HOME", cli.home.path())
                .status()
                .unwrap()
                .success());
        }
        cli
    }

    fn run(&self, options: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        command
            .arg("run")
            .args(options)
            .args(["--", COMMAND])
            .current_dir(self.repo.path())
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("CONTEXTSTREAM_API_URL", &self.url)
            .env("CONTEXTSTREAM_API_KEY", KEY)
            .env("CONTEXTSTREAM_WORKSPACE_ID", WORKSPACE)
            .kill_on_drop(true);
        command
    }

    fn hints_dir(&self) -> std::path::PathBuf {
        self.home.path().join(".contextstream")
    }

    fn cache_file(&self, start: &str) -> std::path::PathBuf {
        std::fs::read_dir(self.hints_dir())
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name().to_string_lossy().starts_with(start))
            .unwrap()
            .path()
    }
}

async fn serve(api: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    url
}

fn catalog() -> Value {
    json!({"tools": [{
        "name": "check", "description": "Remote checks", "input_schema": {"type": "object"},
        "transport": {"kind": "stream"}, "attachments": ["checkout_snapshot"],
        "command_hints": {"argument": "command", "patterns": [format!("^{COMMAND}$")]}
    }]})
}

async fn streaming_api(worker_url: String) -> (String, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let count = reads.clone();
    let url = serve(
        Router::new()
            .route(
                CATALOG,
                get(move |headers: HeaderMap, uri: Uri| async move {
                    assert_eq!(headers["x-api-key"], KEY);
                    assert_eq!(
                        uri.query(),
                        Some(format!("workspace_id={WORKSPACE}").as_str())
                    );
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"success": true, "data": catalog()}))
                }),
            )
            .route(
                CALL,
                post(move |Json(body): Json<Value>| async move {
                    assert_eq!(body["workspace_id"], WORKSPACE);
                    assert_eq!(body["arguments"]["command"], COMMAND);
                    assert!(body["checkout"].is_object());
                    Json(json!({
                        "call": {"id": CALL_ID},
                        "stream": {
                            "url": worker_url, "token": "one-call", "protocol": "frames/1",
                            "request": {"id": CALL_ID, "repo_key": "test-repo",
                                "command": COMMAND, "timeout_s": 10, "wait_s": 5}
                        }
                    }))
                }),
            ),
    )
    .await;
    (url, reads)
}

async fn worker(results: Vec<Value>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        for result in results {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut buffer = FrameBuffer::default();
            let mut received = Vec::new();
            while received.len() < 2 {
                let Message::Binary(bytes) = socket.next().await.unwrap().unwrap() else {
                    panic!("expected a checkout frame");
                };
                buffer.push(&bytes);
                while let Some(frame) = buffer.next_frame().unwrap() {
                    received.push(frame);
                }
            }
            let (kind, hello) = &received[0];
            assert_eq!(*kind, b'J');
            assert_eq!(
                serde_json::from_slice::<Value>(hello).unwrap()["id"],
                CALL_ID
            );
            assert_eq!(received[1].0, b'D');
            if result["ran"] == true {
                socket
                    .send(Message::binary(encode_frame(b'O', b"remote-output")))
                    .await
                    .unwrap();
            }
            socket
                .send(Message::binary(encode_frame(
                    b'J',
                    result.to_string().as_bytes(),
                )))
                .await
                .unwrap();
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
        }
    });
    (url, task)
}

#[tokio::test]
async fn hosted_run_fetches_missing_and_stale_hints_and_runs_remotely() {
    let result = json!({"type": "result", "ran": true, "exit": 23});
    let (worker_url, worker) = worker(vec![result; 3]).await;
    let (api, reads) = streaming_api(worker_url).await;
    let cli = Cli::new(api);
    assert!(!cli.hints_dir().exists());
    for (index, expected_gets) in [1, 1, 2].into_iter().enumerate() {
        if index == 2 {
            let path = cli.cache_file(command_hints::CACHE_FILE);
            let mut old: Cache = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            old.written_at -= 301;
            std::fs::write(path, serde_json::to_vec(&old).unwrap()).unwrap();
        }
        let output = cli.run(&[]).output().await.unwrap();
        assert_eq!(output.status.code(), Some(23), "{output:?}");
        assert_eq!(output.stdout, b"remote-output");
        assert_eq!(reads.load(Ordering::SeqCst), expected_gets);
        let path = cli.cache_file(command_hints::CACHE_FILE);
        let hints = command_hints::read_from(&path, &command_hints::local_identity(KEY), &cli.url)
            .expect("run shared the hints with the editor hook");
        assert!(hints.is_fresh(mcp_client::hosted_tools::KNOWN_TTL));
        assert_eq!(hints.tool_for(COMMAND).unwrap().name, "check");
    }
    worker.await.unwrap();
}

#[tokio::test]
async fn hosted_run_verbose_shows_the_call_id_and_the_decline() {
    let reason = "the session runs Node 26 and this host runs Node 22 or 24 or 20";
    let result = json!({"type": "result", "ran": false, "exit": 75,
        "refused": format!("\0{reason}, so it runs in the session\r\n")});
    let (worker_url, worker) = worker(vec![result.clone(), result]).await;
    let (api, _) = streaming_api(worker_url).await;
    let cli = Cli::new(api);
    for verbose in [false, true] {
        let options = if verbose { &["-v"][..] } else { &[] };
        let output = cli.run(options).output().await.unwrap();
        assert_local(&output);
        let text = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            text.lines()
                .any(|line| line == format!("contextstream: remote call {CALL_ID}")),
            verbose,
            "{text}"
        );
        assert!(
            text.lines()
                .any(|line| line == format!("contextstream: running it here instead ({reason})")),
            "{text}"
        );
    }
    worker.await.unwrap();
}

#[tokio::test]
async fn hosted_run_respects_off_and_remembers_timed_out_and_failed_catalog_reads() {
    let reads = Arc::new(AtomicUsize::new(0));
    let count = reads.clone();
    let api = serve(Router::new().route(
        CATALOG,
        get(move || async move {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                std::future::pending().await
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    ))
    .await;
    let cli = Cli::new(api);
    let off = cli
        .run(&[])
        .env(command_hints::ENV_SWITCH, "off")
        .output()
        .await
        .unwrap();
    assert_local(&off);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(!cli.hints_dir().exists());

    for (index, expected_gets) in [1, 1, 2, 2].into_iter().enumerate() {
        if index == 2 {
            std::fs::File::open(cli.cache_file("remote-checks-catalog-unreachable-"))
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(31))
                .unwrap();
        }
        let output = tokio::time::timeout(Duration::from_secs(4), cli.run(&[]).output())
            .await
            .expect("the catalog wait was bounded")
            .unwrap();
        assert_local(&output);
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(reads.load(Ordering::SeqCst), expected_gets);
    }
}

#[tokio::test]
async fn hosted_run_explains_api_refusals_before_a_stream_opens() {
    for message in [
        "This tool is not available for your account.",
        "Additional check engines are off for this account.",
    ] {
        let api = serve(
            Router::new()
                .route(CATALOG, get(|| async { Json(catalog()) }))
                .route(
                    CALL,
                    post(move || async move {
                        (
                            StatusCode::FORBIDDEN,
                            Json(json!({
                                "success": false, "error": {"code": "FORBIDDEN", "message": message}
                            })),
                        )
                    }),
                ),
        )
        .await;
        let output = Cli::new(api).run(&[]).output().await.unwrap();
        assert_local(&output);
        assert_eq!(
            String::from_utf8(output.stderr).unwrap().trim(),
            format!("contextstream: running it here instead ({message})")
        );
    }
}

fn assert_local(output: &Output) {
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(output.stdout, b"local-output");
}
