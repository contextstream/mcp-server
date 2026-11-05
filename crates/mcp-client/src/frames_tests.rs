use super::*;
use std::io::Read;
use std::path::Path;
use std::process::Command as Std;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpListener;

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Std::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, rel: &str, content: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A repository with one commit and one local change (`a.txt`), plus `extra`
/// bytes in a new file `big.bin` when there are some.
fn changed_repo(extra: usize) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    git_ok(dir, &["init", "-q"]);
    write(dir, "a.txt", b"alpha\n");
    write(dir, "src/lib.rs", b"pub fn one() {}\n");
    git_ok(dir, &["add", "-A"]);
    git_ok(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "first",
        ],
    );
    write(dir, "a.txt", b"alpha, changed here\n");
    if extra > 0 {
        // Bytes git cannot squeeze: a pack of them stays about as large.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let bytes: Vec<u8> = (0..extra)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        write(dir, "big.bin", &bytes);
    }
    temp
}

fn request() -> Value {
    json!({
        "id": "call-1", "command": "run the thing", "cwd": "src", "env_names": ["TZ", "API_TOKEN"],
        "timeout_s": 120, "wait_s": 60, "pipefail": true, "repo_key": "repo-key",
        "unexpected": "never copied",
    })
}

/// The service's end of a socket.
struct Worker {
    ws: WebSocketStream<DuplexStream>,
    buffer: FrameBuffer,
    /// The size of each message the client sent, in order.
    message_sizes: Vec<usize>,
}

impl Worker {
    async fn recv(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            if let Some(frame) = self.buffer.next_frame().unwrap() {
                return Some(frame);
            }
            match self.ws.next().await? {
                Ok(Message::Binary(bytes)) => {
                    self.message_sizes.push(bytes.len());
                    self.buffer.push(&bytes);
                }
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn send(&mut self, kind: u8, payload: &[u8]) {
        let frame = encode_frame(kind, payload);
        self.ws
            .send(Message::binary(frame))
            .await
            .expect("worker send");
    }

    async fn send_json(&mut self, value: Value) {
        self.send(b'J', value.to_string().as_bytes()).await;
    }
}

/// A client socket and the service's end of it.
async fn pair() -> (WebSocketStream<DuplexStream>, Worker) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    (
        client,
        Worker {
            ws: server,
            buffer: FrameBuffer::default(),
            message_sizes: Vec::new(),
        },
    )
}

async fn hello_for(snapshot: &CheckoutSnapshot) -> Value {
    hello(
        &request(),
        snapshot.request_fields(),
        Map::new(),
        Delivery::ToolResult,
    )
    .unwrap()
}

#[test]
fn frames_are_cut_from_a_byte_stream() {
    let mut buffer = FrameBuffer::default();
    let whole = [encode_frame(b'J', b"{}"), encode_frame(b'O', b"hello")].concat();
    // A frame split anywhere comes out whole, once, when its last byte is in.
    for (index, byte) in whole[..whole.len() - 1].iter().enumerate() {
        buffer.push(&[*byte]);
        let got = buffer.next_frame().unwrap();
        if index == 6 {
            assert_eq!(got, Some((b'J', b"{}".to_vec())), "at byte {index}");
        } else {
            assert_eq!(got, None, "at byte {index}");
        }
    }
    buffer.push(&whole[whole.len() - 1..]);
    assert_eq!(
        buffer.next_frame().unwrap(),
        Some((b'O', b"hello".to_vec()))
    );
    assert_eq!(buffer.next_frame().unwrap(), None);
    // A frame too large to be real is refused before it is read.
    let mut huge = vec![b'O'];
    huge.extend_from_slice(&((FRAME_MAX as u32) + 1).to_be_bytes());
    buffer.push(&huge);
    assert!(matches!(
        buffer.next_frame(),
        Err(ExchangeError::Protocol(_))
    ));
}

#[test]
fn a_stream_is_wss_or_this_machine() {
    assert!(stream_url_allowed("wss://example.com/v1/run"));
    assert!(stream_url_allowed("ws://127.0.0.1:9/run"));
    assert!(stream_url_allowed("ws://localhost:9/run"));
    assert!(stream_url_allowed("ws://[::1]:9/run"));
    assert!(!stream_url_allowed("ws://example.com/run"));
    assert!(!stream_url_allowed("https://example.com/run"));
    assert!(!stream_url_allowed("http://127.0.0.1/run"));
    assert!(!stream_url_allowed("file:///etc/passwd"));
    assert!(!stream_url_allowed("wss://"));
    assert!(!stream_url_allowed("not a url"));
}

#[test]
fn the_environment_is_added_to_a_hello_when_there_is_one() {
    let base = hello(&request(), Map::new(), Map::new(), Delivery::Buffered).unwrap();
    let environment = json!({"platform": {"os": "linux", "arch": "x86_64"}, "versions": {}});
    let with = with_environment(base.clone(), Some(environment.clone()));
    assert_eq!(with["environment"], environment);
    assert_eq!(with["buffered"], true);
    let mut without_it = with.clone();
    without_it.as_object_mut().unwrap().remove("environment");
    assert_eq!(without_it, base);
    // Switched off: the hello is what it was.
    assert_eq!(with_environment(base.clone(), None), base);
}

#[test]
fn the_hello_holds_what_the_api_decided_and_nothing_it_did_not() {
    let mut fields = Map::new();
    fields.insert("base_tree".into(), json!("b"));
    fields.insert("target_tree".into(), json!("t"));
    let mut env = Map::new();
    env.insert("TZ".into(), json!("UTC"));
    let hello = hello(&request(), fields, env, Delivery::ToolResult).unwrap();
    assert_eq!(
        hello,
        json!({
            "type": "hello", "v": 1, "id": "call-1", "repo_key": "repo-key", "cwd": "src",
            "command": "run the thing", "timeout_s": 120, "wait_s": 60, "pipefail": true,
            "base_tree": "b", "target_tree": "t", "env": {"TZ": "UTC"}, "tool_result": true,
        })
    );
    let without = super::hello(&request(), Map::new(), Map::new(), Delivery::Streamed).unwrap();
    assert!(without.get("tool_result").is_none());
    assert!(without.get("buffered").is_none());
    // A caller that takes the output at the end says so, and a service-made
    // result is never also called buffered.
    let buffered = super::hello(&request(), Map::new(), Map::new(), Delivery::Buffered).unwrap();
    assert_eq!(buffered["buffered"], true);
    assert!(buffered.get("tool_result").is_none());
    assert!(hello.get("buffered").is_none());
    for missing in ["id", "repo_key", "command", "timeout_s", "wait_s"] {
        let mut request = request();
        request.as_object_mut().unwrap().remove(missing);
        let error = super::hello(&request, Map::new(), Map::new(), Delivery::Streamed).unwrap_err();
        assert!(error.contains(missing), "{error}");
    }
    // A cwd the API leaves out is the root of the repository.
    let mut request = request();
    request.as_object_mut().unwrap().remove("cwd");
    let root = super::hello(&request, Map::new(), Map::new(), Delivery::Streamed).unwrap();
    assert_eq!(root["cwd"], "");
}

#[test]
fn only_plain_names_that_hold_no_credentials_are_forwarded() {
    for ok in ["TZ", "LANG", "NODE_OPTIONS", "RUST_LOG", "_X1"] {
        assert!(env_name_forwardable(ok), "{ok}");
    }
    for bad in [
        "",
        "1X",
        "A-B",
        "A B",
        "AWS_SECRET_ACCESS_KEY",
        "API_TOKEN",
        "GITHUB_TOKEN",
        "DB_PASSWORD",
        "SSH_AUTH_SOCK",
        "CONTEXTSTREAM_API_KEY",
        "contextstream_url",
        "NPM_AUTH",
        "x".repeat(65).as_str(),
    ] {
        assert!(!env_name_forwardable(bad), "{bad}");
    }
    let values = |name: &str| match name {
        "TZ" => Some("UTC".to_string()),
        "API_TOKEN" => Some("not-sent".to_string()),
        "PROXY" => Some("http://user:pw@proxy.example:8080".to_string()),
        "BIG" => Some("x".repeat(5000)),
        "NUL" => Some("a\0b".to_string()),
        "PLAIN_URL" => Some("https://example.com/a?b=c@d".to_string()),
        _ => None,
    };
    let request =
        json!({"env_names": ["TZ", "API_TOKEN", "PROXY", "BIG", "NUL", "PLAIN_URL", "UNSET", 7]});
    let env = forwarded_env(&request, values);
    let mut names: Vec<_> = env.keys().cloned().collect();
    names.sort();
    assert_eq!(names, ["PLAIN_URL", "TZ"]);
    // At most 32 are sent.
    let many: Vec<String> = (0..50).map(|i| format!("V{i}")).collect();
    let env = forwarded_env(&json!({ "env_names": many }), |_| Some("1".to_string()));
    assert_eq!(env.len(), 32);
}

#[test]
fn a_call_is_given_its_limits_plus_slack() {
    assert_eq!(
        deadline_of(&json!({"timeout_s": 100, "wait_s": 20})),
        Duration::from_secs(120) + DEADLINE_SLACK
    );
    assert_eq!(deadline_of(&json!({})), DEADLINE_SLACK);
    assert_eq!(
        deadline_of(&json!({"timeout_s": -5, "wait_s": "x"})),
        DEADLINE_SLACK
    );
}

#[tokio::test]
async fn a_call_sends_its_hello_and_pack_answers_needs_and_returns_the_result() {
    let repo = changed_repo(0);
    let snapshot = CheckoutSnapshot::capture(repo.path()).await.unwrap();
    let hello = hello_for(&snapshot).await;
    let (client, mut worker) = pair().await;
    let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();

    let service = tokio::spawn(async move {
        let (kind, payload) = worker.recv().await.unwrap();
        assert_eq!(kind, b'J');
        let hello: Value = serde_json::from_slice(&payload).unwrap();
        let (kind, pack) = worker.recv().await.unwrap();
        assert_eq!(kind, b'D');
        assert_eq!(&pack[..4], b"PACK", "the first pack is a git pack");
        worker
            .send_json(json!({"type": "info", "text": "accepted"}))
            .await;
        // It asks for the base listing, then for the blob of a file it lacks.
        worker
            .send_json(json!({"type": "need", "listing": true}))
            .await;
        let (kind, listing) = worker.recv().await.unwrap();
        assert_eq!(kind, b'D');
        let mut text = String::new();
        flate2::read::ZlibDecoder::new(&listing[..])
            .read_to_string(&mut text)
            .unwrap();
        assert!(text.contains("src/lib.rs"), "{text:?}");
        let blob = hello["overlay"][0][1].as_str().unwrap().to_string();
        worker
            .send_json(json!({"type": "need", "blobs": [blob]}))
            .await;
        let (kind, blobs) = worker.recv().await.unwrap();
        assert_eq!(kind, b'D');
        assert_eq!(&blobs[..4], b"PACK");
        worker.send(b'O', b"some output").await;
        worker
            .send_json(
                json!({"type": "result", "ran": true, "exit": 0, "tool_result": {"content": []}}),
            )
            .await;
        hello
    });

    let result = exchange(client, &hello, &snapshot, Duration::from_secs(30), events)
        .await
        .unwrap();
    assert_eq!(result["ran"], true);
    assert_eq!(result["exit"], 0);
    let sent = service.await.unwrap();
    assert_eq!(sent["type"], "hello");
    assert_eq!(sent["tool_result"], true);
    assert_eq!(sent["base_tree"], snapshot.request_fields()["base_tree"]);
    assert_eq!(sent["overlay"][0][2], "a.txt");
    let mut got = Vec::new();
    while let Ok(event) = seen.try_recv() {
        got.push(event);
    }
    assert_eq!(
        got,
        vec![Event::Heartbeat, Event::Output(b"some output".to_vec())]
    );
}

#[tokio::test]
async fn a_big_pack_goes_in_messages_of_a_mebibyte_at_most() {
    let repo = changed_repo(2_500_000);
    let snapshot = CheckoutSnapshot::capture(repo.path()).await.unwrap();
    let hello = hello_for(&snapshot).await;
    let (client, mut worker) = pair().await;
    let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
    let service = tokio::spawn(async move {
        worker.recv().await.unwrap();
        let (kind, pack) = worker.recv().await.unwrap();
        assert_eq!(kind, b'D');
        worker
            .send_json(json!({"type": "result", "ran": true, "exit": 0}))
            .await;
        (pack.len(), worker.message_sizes)
    });
    exchange(client, &hello, &snapshot, Duration::from_secs(30), events)
        .await
        .unwrap();
    let (pack_len, sizes) = service.await.unwrap();
    assert!(pack_len > 2_000_000, "{pack_len}");
    assert!(sizes.len() >= 4, "{sizes:?}");
    assert!(sizes.iter().all(|size| *size <= MESSAGE_MAX), "{sizes:?}");
}

#[tokio::test(start_paused = true)]
async fn a_service_that_never_speaks_is_given_up_on() {
    let repo = changed_repo(0);
    let snapshot = CheckoutSnapshot::capture(repo.path()).await.unwrap();
    let hello = hello_for(&snapshot).await;
    let (client, _worker) = pair().await;
    let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
    let started = tokio::time::Instant::now();
    let error = exchange(client, &hello, &snapshot, Duration::from_secs(600), events)
        .await
        .unwrap_err();
    assert!(matches!(error, ExchangeError::Silent(_)), "{error}");
    assert_eq!(started.elapsed(), FIRST_FRAME_WAIT);
}

#[tokio::test(start_paused = true)]
async fn a_call_that_runs_past_its_limits_is_ended() {
    let repo = changed_repo(0);
    let snapshot = CheckoutSnapshot::capture(repo.path()).await.unwrap();
    let hello = hello_for(&snapshot).await;
    let (client, mut worker) = pair().await;
    let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let service = tokio::spawn(async move {
        worker.recv().await.unwrap();
        worker.recv().await.unwrap();
        // One heartbeat, then silence for good.
        worker.send_json(json!({"type": "info"})).await;
        std::future::pending::<()>().await;
    });
    let error = exchange(client, &hello, &snapshot, Duration::from_secs(90), events)
        .await
        .unwrap_err();
    assert!(matches!(error, ExchangeError::Deadline), "{error}");
    assert_eq!(seen.try_recv().ok(), Some(Event::Heartbeat));
    service.abort();
}

#[tokio::test]
async fn a_connection_that_ends_early_is_reported() {
    let repo = changed_repo(0);
    let snapshot = CheckoutSnapshot::capture(repo.path()).await.unwrap();
    let hello = hello_for(&snapshot).await;
    let (client, mut worker) = pair().await;
    let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
    let service = tokio::spawn(async move {
        worker.recv().await.unwrap();
        worker.send_json(json!({"type": "info"})).await;
        drop(worker);
    });
    let error = exchange(client, &hello, &snapshot, Duration::from_secs(30), events)
        .await
        .unwrap_err();
    assert!(
        matches!(error, ExchangeError::Ended | ExchangeError::Wire(_)),
        "{error}"
    );
    service.await.unwrap();
}

#[tokio::test]
async fn a_request_for_something_not_in_the_checkout_ends_the_call() {
    let repo = changed_repo(0);
    let snapshot = CheckoutSnapshot::capture(repo.path()).await.unwrap();
    let hello = hello_for(&snapshot).await;
    let (client, mut worker) = pair().await;
    let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
    let service = tokio::spawn(async move {
        worker.recv().await.unwrap();
        worker.recv().await.unwrap();
        // A well-formed id that names nothing this checkout holds.
        worker
            .send_json(
                json!({"type": "need", "blobs": ["0123456789abcdef0123456789abcdef01234567"]}),
            )
            .await;
        std::future::pending::<()>().await;
    });
    let error = exchange(client, &hello, &snapshot, Duration::from_secs(30), events)
        .await
        .unwrap_err();
    assert!(matches!(error, ExchangeError::Snapshot(_)), "{error}");
    service.abort();
}

#[tokio::test]
async fn frame_types_it_does_not_know_are_skipped() {
    let repo = changed_repo(0);
    let snapshot = CheckoutSnapshot::capture(repo.path()).await.unwrap();
    let hello = hello_for(&snapshot).await;
    let (client, mut worker) = pair().await;
    let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
    let service = tokio::spawn(async move {
        worker.recv().await.unwrap();
        worker.recv().await.unwrap();
        worker.send(b'A', b"an artifact").await;
        worker.send(b'Z', b"from a newer service").await;
        worker.send_json(json!({"type": "future-kind"})).await;
        worker
            .send_json(json!({"type": "result", "ran": false, "exit": 75}))
            .await;
    });
    let result = exchange(client, &hello, &snapshot, Duration::from_secs(30), events)
        .await
        .unwrap();
    assert_eq!(result["ran"], false);
    assert_eq!(result["exit"], 75);
    service.await.unwrap();
}

/// A one-connection HTTP server on this machine: calls `serve` with the raw
/// request head and the stream.
async fn serve_once<F, Fut>(serve: F) -> String
where
    F: FnOnce(String, tokio::net::TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap() == 0 {
                return;
            }
            head.push(byte[0]);
        }
        serve(String::from_utf8_lossy(&head).into_owned(), stream).await;
    });
    format!("ws://{address}/v1/run")
}

#[tokio::test]
async fn the_socket_is_opened_with_the_call_token_and_nothing_else() {
    let url = serve_once(|head, stream| async move {
        let lower = head.to_ascii_lowercase();
        assert!(lower.starts_with("get /v1/run http/1.1"), "{head}");
        assert!(lower.contains("authorization: bearer call-token\r\n"), "{head}");
        assert!(lower.contains("upgrade: websocket\r\n"), "{head}");
        assert!(lower.contains("sec-websocket-version: 13\r\n"), "{head}");
        // The head carries no cookie and no other credential.
        assert!(!lower.contains("cookie"), "{head}");
        assert_eq!(lower.matches("authorization").count(), 1, "{head}");
        let key = head
            .lines()
            .find_map(|line| line.strip_prefix("sec-websocket-key: ").or_else(|| line.strip_prefix("Sec-WebSocket-Key: ")))
            .unwrap()
            .trim()
            .to_string();
        let accept = derive_accept_key(key.as_bytes());
        let mut stream = stream;
        stream
            .write_all(
                format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
        ws.send(Message::binary(encode_frame(b'O', b"hi"))).await.unwrap();
        // The client sends one message back.
        let reply = ws.next().await.unwrap().unwrap();
        assert_eq!(reply.into_data().as_ref(), b"pong");
    })
    .await;
    let mut ws = connect(&url, "call-token").await.unwrap();
    let message = ws.next().await.unwrap().unwrap();
    assert_eq!(message.into_data().as_ref(), encode_frame(b'O', b"hi"));
    ws.send(Message::binary(b"pong".to_vec())).await.unwrap();
    // Let the server see it before the test ends.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn an_answer_that_is_not_an_upgrade_is_an_error_and_a_redirect_is_not_followed() {
    for (status, expect) in [
        ("401 Unauthorized", "401"),
        ("302 Found", "302"),
        ("409 Conflict", "409"),
    ] {
        let url = serve_once(move |_, mut stream| async move {
            stream
                .write_all(
                    format!("HTTP/1.1 {status}\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
        })
        .await;
        let error = connect(&url, "call-token").await.err().unwrap();
        let ExchangeError::Wire(text) = &error else {
            panic!("{error}");
        };
        assert!(text.contains(expect), "{text}");
        assert!(!text.contains("call-token"));
    }
}

#[tokio::test]
async fn an_upgrade_answer_that_does_not_match_the_request_is_refused() {
    let url = serve_once(|_, mut stream| async move {
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: AAAAAAAAAAAAAAAAAAAAAAAAAAA=\r\n\r\n",
            )
            .await
            .unwrap();
    })
    .await;
    let error = connect(&url, "call-token").await.err().unwrap();
    assert!(matches!(error, ExchangeError::Protocol(_)), "{error}");
}

#[tokio::test]
async fn an_address_that_is_not_wss_is_not_connected_to() {
    for url in ["http://127.0.0.1:1/run", "ws://example.com/run", "garbage"] {
        let error = connect(url, "call-token").await.err().unwrap();
        assert!(
            matches!(error, ExchangeError::Protocol(_)),
            "{url}: {error}"
        );
    }
}
