//! Drive the real gateway (`contextstream-mcp http`) against a fake API: whose
//! session a request belongs to when two callers present the same
//! `Mcp-Session-Id`.
//!
//! The gateway runs as the hosted one does: it asks every request for a
//! credential, and it holds no secret to verify a bearer token with (the API
//! verifies it). A session's scope (its folder, workspace and project) is
//! what a tool call that names none is addressed to, so a scope two callers
//! share sends one caller's reads and writes to the other's workspace.
//!
//! - A signed-in caller that presents another signed-in caller's session id
//!   gets a session of its own: nothing of the other's scope, and it changes
//!   nothing of it. It is answered exactly as with an id nobody uses.
//! - A caller whose token was replaced (the same subject, a new token the
//!   API accepts) keeps its session.
//! - A token that names another caller's subject and that the API refuses,
//!   or could not be asked about, is not that caller: it gets nothing of the
//!   caller's session, not even its ids, and is answered as with an id
//!   nobody uses.
//! - Callers with API keys were kept apart before, and still are.
//!
//! Every gateway gets its own HOME; the API is a listener on localhost that
//! resolves a session that names no workspace to the caller's own, by the
//! credential the request carries, as the API does.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A caller: its credential header, and the workspace and project the API
/// resolves for it.
#[derive(Clone)]
struct Caller {
    header: String,
    credential: String,
    workspace: &'static str,
    project: &'static str,
    folder: &'static str,
}

const ANN_SCOPE: (&str, &str, &str) = (
    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
    "/home/ann/acme-plans",
);
const BOB_SCOPE: (&str, &str, &str) = (
    "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
    "/home/bob/other-work",
);

impl Caller {
    fn new(
        header: &str,
        credential: String,
        scope: (&'static str, &'static str, &'static str),
    ) -> Self {
        Self {
            header: header.to_string(),
            credential,
            workspace: scope.0,
            project: scope.1,
            folder: scope.2,
        }
    }

    /// Signed in: a bearer token shaped as the API's are (three segments,
    /// RS256) that names `subject`, with a made-up signature. Nothing here
    /// can verify it, as on the hosted gateway.
    fn signed_in(
        subject: &str,
        signature: &str,
        scope: (&'static str, &'static str, &'static str),
    ) -> Self {
        let token = format!(
            "{}.{}.{signature}",
            base64url(br#"{"alg":"RS256","typ":"JWT"}"#),
            base64url(json!({ "sub": subject }).to_string().as_bytes())
        );
        Self::new("Authorization", format!("Bearer {token}"), scope)
    }

    fn with_key(key: &str, scope: (&'static str, &'static str, &'static str)) -> Self {
        Self::new("X-API-Key", key.to_string(), scope)
    }

    /// Whether `text` carries anything of this caller's scope.
    fn is_named_in(&self, text: &str) -> bool {
        [self.workspace, self.project, self.folder]
            .iter()
            .any(|own| text.contains(own))
    }
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * index) & 63) as usize] as char);
        }
    }
    out
}

/// What the fake API was asked: under which credential, and everything of
/// the request that can carry a workspace or a project.
#[derive(Clone, Debug)]
struct Asked {
    credential: String,
    line: String,
    said: String,
}

/// A stand-in for the ContextStream API. It keeps what it was asked, and
/// resolves a session that names no workspace to the workspace and project
/// of the caller whose credential the request carries. It refuses (401)
/// every request under a token whose signature is `forged`, as the API
/// refuses a token it did not issue, and fails (500) to say who a token
/// signed `unanswered` is.
#[derive(Clone)]
struct FakeApi {
    port: u16,
    asked: Arc<Mutex<Vec<Asked>>>,
    callers: Arc<Mutex<Vec<Caller>>>,
}

impl FakeApi {
    fn start(callers: &[&Caller]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = Self {
            port: listener.local_addr().unwrap().port(),
            asked: Arc::new(Mutex::new(Vec::new())),
            callers: Arc::new(Mutex::new(callers.iter().map(|c| (*c).clone()).collect())),
        };
        let serving = api.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let serving = serving.clone();
                std::thread::spawn(move || serving.serve(stream));
            }
        });
        api
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }

    fn serve(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        let (head_end, length) = loop {
            let Ok(read) = stream.read(&mut chunk) else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]).into_owned();
                let length = header(&head, "content-length")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                break (end + 4, length);
            }
        };
        while request.len() < head_end + length {
            match stream.read(&mut chunk) {
                Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
                _ => break,
            }
        }
        let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
        let line = head.lines().next().unwrap_or_default().to_string();
        let path = line.split(' ').nth(1).unwrap_or_default().to_string();
        let credential = header(&head, "x-api-key")
            .or_else(|| header(&head, "authorization"))
            .unwrap_or_default()
            .to_string();
        let body_text = String::from_utf8_lossy(&request[head_end..]).into_owned();
        let body: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
        let route = path.split('?').next().unwrap_or_default();
        let status = if credential.ends_with(".forged") {
            "401 Unauthorized"
        } else if credential.ends_with(".unanswered") && route.ends_with("/auth/me") {
            "500 Internal Server Error"
        } else {
            "200 OK"
        };
        let data = if route.ends_with("/auth/me") {
            json!({"id": "99999999-9999-4999-8999-999999999999", "email": "qa@example.com", "created_at": "2026-01-01T00:00:00Z"})
        } else if route.contains("/session/init") {
            // The caller's own workspace and project, unless the request
            // names one.
            let own = self
                .callers
                .lock()
                .unwrap()
                .iter()
                .find(|caller| caller.credential == credential)
                .map(|caller| (caller.workspace, caller.project));
            let workspace = body["workspace_id"].as_str().or(own.map(|own| own.0));
            let project = body["project_id"].as_str().or(own.map(|own| own.1));
            json!({
                "session_id": "55555555-5555-4555-8555-555555555555",
                "workspace_id": workspace,
                "project_id": project,
                "workspace": {"id": workspace, "name": "A workspace"},
                "project": {"id": project, "name": "a-project"},
                "workspace_resolution": "requested",
                "status": "ok",
            })
        } else if route.contains("/workspaces/") || route.contains("/projects/") {
            json!({"id": route.split('/').nth(4), "name": "A name"})
        } else {
            json!({})
        };
        self.asked.lock().unwrap().push(Asked {
            credential,
            said: format!(
                "{path} {body_text} {}",
                header(&head, "x-workspace-id").unwrap_or_default()
            ),
            line,
        });
        let text = if status == "200 OK" {
            json!({"success": true, "data": data}).to_string()
        } else {
            json!({"success": false, "error": {"message": "no"}}).to_string()
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            text.len(),
            text
        );
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A gateway process, as hosted: every request needs a credential, and a
/// bearer token is the API's to verify.
struct Gateway {
    port: u16,
    process: Child,
    api: FakeApi,
    _home: tempfile::TempDir,
}

impl Gateway {
    fn start(api: &FakeApi) -> Self {
        // A free port is found by binding it and letting go, and the gateway
        // binds it a moment later. One gateway starts at a time, so no two
        // tests of this process are handed the same port; a gateway that
        // lost its port to another process exits, and is started again.
        static STARTING: Mutex<()> = Mutex::new(());
        let _starting = STARTING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for _ in 0..5 {
            if let Some(gateway) = Self::try_start(api) {
                return gateway;
            }
        }
        panic!("the gateway did not start");
    }

    fn try_start(api: &FakeApi) -> Option<Self> {
        let home = tempfile::tempdir().unwrap();
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let process = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "http",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "-q",
            ])
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env(
                "CONTEXTSTREAM_API_URL",
                format!("http://127.0.0.1:{}", api.port),
            )
            .env("MCP_HTTP_REQUIRE_AUTH", "true")
            .env("CONTEXTSTREAM_ALLOW_HEADER_AUTH", "true")
            // An unoptimised build's worker threads overflow their default
            // stack in the deeper tools (`search`); give them room.
            .env("RUST_MIN_STACK", (256 * 1024 * 1024).to_string())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut gateway = Self {
            port,
            process,
            api: api.clone(),
            _home: home,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // Gone: it could not bind the port.
            if gateway.process.try_wait().unwrap().is_some() {
                return None;
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                // The port answers; it is this gateway when the process is
                // still there a moment later.
                std::thread::sleep(Duration::from_millis(300));
                return gateway
                    .process
                    .try_wait()
                    .unwrap()
                    .is_none()
                    .then_some(gateway);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// One request to `/mcp`: the whole answer, head and body.
    fn post(&self, caller: &Caller, session_id: Option<&str>, body: &Value) -> String {
        let text = body.to_string();
        let mut request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n{}: {}\r\n",
            self.port,
            text.len(),
            caller.header,
            caller.credential,
        );
        if let Some(session_id) = session_id {
            request.push_str(&format!("Mcp-Session-Id: {session_id}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(&text);
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// `initialize`: the session id the gateway issues.
    fn initialize(&self, caller: &Caller) -> String {
        let answer = self.post(
            caller,
            None,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "claude-code", "version": "1"},
            }}),
        );
        let head = answer.split("\r\n\r\n").next().unwrap_or_default();
        header(head, "mcp-session-id")
            .expect("initialize issues a session id")
            .to_string()
    }

    /// A tool call of `caller` in the session `session_id`: what the caller
    /// reads back, and what the API was asked under the caller's credential
    /// from then on. (What an earlier call of another caller started on the
    /// side may still arrive meanwhile, under that caller's credential; it
    /// is not this call's.)
    fn call(&self, caller: &Caller, session_id: &str, tool: &str, arguments: Value) -> Outcome {
        let before = self.api.asked().len();
        let answer = self.post(
            caller,
            Some(session_id),
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments}}),
        );
        // What a call starts on the side has arrived once the API is quiet.
        let mut seen = self.api.asked().len();
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let now = self.api.asked().len();
            if now == seen {
                break;
            }
            seen = now;
        }
        assert!(
            answer.starts_with("HTTP/1.1 200"),
            "{tool}: {}",
            answer.lines().next().unwrap_or("(no answer)")
        );
        Outcome {
            answer,
            asked: self
                .api
                .asked()
                .into_iter()
                .skip(before)
                .filter(|asked| asked.credential == caller.credential)
                .collect(),
        }
    }

    fn search(&self, caller: &Caller, session_id: &str) -> Outcome {
        self.call(
            caller,
            session_id,
            "search",
            json!({"query": "release plan", "mode": "keyword"}),
        )
    }

    fn save_a_note(&self, caller: &Caller, session_id: &str) -> Outcome {
        self.call(
            caller,
            session_id,
            "session",
            json!({"action": "capture", "event_type": "note", "title": "a note", "content": "a note"}),
        )
    }

    fn start_a_session(&self, caller: &Caller, session_id: &str) -> Outcome {
        self.call(
            caller,
            session_id,
            "init",
            json!({"folder_path": caller.folder}),
        )
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// What a tool call came to: the answer its caller reads, and what the API
/// was asked.
struct Outcome {
    answer: String,
    asked: Vec<Asked>,
}

impl Outcome {
    /// Whether anything of `caller`'s scope is in the answer or in what the
    /// API was asked.
    fn carries(&self, caller: &Caller) -> bool {
        caller.is_named_in(&self.answer)
            || self
                .asked
                .iter()
                .any(|asked| caller.is_named_in(&asked.said))
    }

    /// Whether the API was asked for a search or a write in `caller`'s
    /// workspace and project.
    fn is_addressed_to(&self, caller: &Caller) -> bool {
        self.asked.iter().any(|asked| {
            asked.said.contains(caller.workspace) && asked.said.contains(caller.project)
        })
    }

    /// The request lines the API saw.
    fn lines(&self) -> Vec<&str> {
        self.asked.iter().map(|asked| asked.line.as_str()).collect()
    }

    /// The request lines the API saw, without the check of the credential:
    /// the gateway asks the API about a token once, whichever session id
    /// comes with it, so where that check falls says nothing of the id.
    fn work(&self) -> Vec<&str> {
        self.lines()
            .into_iter()
            .filter(|line| !line.contains("/api/v1/auth/me"))
            .collect()
    }
}

/// Ticket 11bc49fb item 10. Two signed-in callers present one session id,
/// the one the gateway issued to the first or one a client made up: each has
/// a session of its own.
#[test]
fn a_session_id_another_signed_in_caller_presents_is_a_session_of_its_own() {
    let ann = Caller::signed_in("user-ann", "signature-a", ANN_SCOPE);
    let bob = Caller::signed_in("user-bob", "signature-b", BOB_SCOPE);
    let api = FakeApi::start(&[&ann, &bob]);
    let gateway = Gateway::start(&api);
    let issued = gateway.initialize(&ann);
    for session_id in [issued.as_str(), "a-session-id-a-client-made-up"] {
        let started = gateway.start_a_session(&ann, session_id);
        assert!(
            started.carries(&ann),
            "{session_id}: Ann's session has her scope"
        );

        // Bob presents Ann's session id and has called nothing yet. He is in
        // a session of his own, which has no scope: nothing of Ann's is in
        // what he reads or in what the API is asked for him.
        let unused = format!("{session_id}-that-nobody-uses");
        for (what, with_anns_id, with_an_unused_id) in [
            (
                "a search",
                gateway.search(&bob, session_id),
                gateway.search(&bob, &unused),
            ),
            (
                "a note",
                gateway.save_a_note(&bob, session_id),
                gateway.save_a_note(&bob, &unused),
            ),
        ] {
            assert!(
                !with_anns_id.carries(&ann),
                "{session_id}: {what} of Bob's carries Ann's scope: {:?}",
                with_anns_id.lines()
            );
            // And he is answered as with an id nobody uses: nothing says
            // that the id is someone's.
            assert_eq!(
                with_anns_id.work(),
                with_an_unused_id.work(),
                "{session_id}: {what}"
            );
        }

        // Bob starts a session under that id: it is his, with his scope.
        let started = gateway.start_a_session(&bob, session_id);
        assert!(started.carries(&bob) && !started.carries(&ann));
        let bobs = gateway.search(&bob, session_id);
        assert!(bobs.is_addressed_to(&bob) && !bobs.carries(&ann));

        // Ann's session is as she left it: her calls go to her workspace and
        // project, under her credential, and nothing of Bob's is in them.
        for (what, anns) in [
            ("a search", gateway.search(&ann, session_id)),
            ("a note", gateway.save_a_note(&ann, session_id)),
        ] {
            assert!(
                anns.is_addressed_to(&ann),
                "{session_id}: {what} of Ann's is not addressed to her scope: {:?}",
                anns.lines()
            );
            assert!(
                !anns.carries(&bob),
                "{session_id}: {what} of Ann's carries Bob's scope: {:?}",
                anns.lines()
            );
        }
    }
}

/// A token that names another caller's subject is that caller only once the
/// API accepted it. One the API refuses (made up), and one the API could not
/// be asked about, present Ann's session id and get a session of their own:
/// nothing of Ann's scope is in what they read, not even an id in an error,
/// nor in what the API is asked, and they are answered as with an id nobody
/// uses. Ann's session is as she left it.
#[test]
fn a_token_the_api_did_not_accept_is_not_the_subject_it_names() {
    let ann = Caller::signed_in("user-ann", "signature-a", ANN_SCOPE);
    let api = FakeApi::start(&[&ann]);
    let gateway = Gateway::start(&api);
    let session_id = gateway.initialize(&ann);
    assert!(gateway.start_a_session(&ann, &session_id).carries(&ann));
    let unused = format!("{session_id}-that-nobody-uses");
    for signature in ["forged", "unanswered"] {
        let other = Caller::signed_in("user-ann", signature, BOB_SCOPE);
        for (what, with_anns_id, with_an_unused_id) in [
            (
                "a search",
                gateway.search(&other, &session_id),
                gateway.search(&other, &unused),
            ),
            (
                "a note",
                gateway.save_a_note(&other, &session_id),
                gateway.save_a_note(&other, &unused),
            ),
            (
                "an init",
                gateway.call(&other, &session_id, "init", json!({})),
                gateway.call(&other, &unused, "init", json!({})),
            ),
        ] {
            assert!(
                !with_anns_id.carries(&ann),
                "{signature}: {what} carries Ann's scope: {:?}",
                with_anns_id.lines()
            );
            assert_eq!(
                with_anns_id.work(),
                with_an_unused_id.work(),
                "{signature}: {what}"
            );
        }
        let anns = gateway.search(&ann, &session_id);
        assert!(
            anns.is_addressed_to(&ann),
            "{signature}: {:?}",
            anns.lines()
        );
    }
}

/// A client replaces its access token every hour. The new token names the
/// same subject and the API accepts it, and the session keeps its scope:
/// nothing is resolved anew.
#[test]
fn a_signed_in_session_keeps_its_scope_when_its_token_is_replaced() {
    let ann = Caller::signed_in("user-ann", "signature-1", ANN_SCOPE);
    let replaced = Caller::signed_in("user-ann", "signature-2", ANN_SCOPE);
    // The fake API resolves a scope for the first token only: were the
    // session resolved anew under the new one, it would get no scope.
    let api = FakeApi::start(&[&ann]);
    let gateway = Gateway::start(&api);
    let session_id = gateway.initialize(&ann);
    gateway.start_a_session(&ann, &session_id);
    let after = gateway.search(&replaced, &session_id);
    assert!(
        after.is_addressed_to(&ann),
        "a replaced token lost the session's scope: {:?}",
        after.lines()
    );
    // Under the new token, and without resolving the session anew.
    assert!(!after
        .lines()
        .iter()
        .any(|line| line.contains("/session/init")));
}

/// Callers with API keys that present one session id were kept apart before
/// this fix, and still are.
#[test]
fn two_api_keys_with_one_session_id_stay_apart() {
    let ann = Caller::with_key("ann-key", ANN_SCOPE);
    let bob = Caller::with_key("bob-key", BOB_SCOPE);
    let api = FakeApi::start(&[&ann, &bob]);
    let gateway = Gateway::start(&api);
    let session_id = "one-session-id";
    gateway.start_a_session(&ann, session_id);
    assert!(!gateway.search(&bob, session_id).carries(&ann));
    gateway.start_a_session(&bob, session_id);
    let anns = gateway.search(&ann, session_id);
    assert!(anns.is_addressed_to(&ann) && !anns.carries(&bob));
}
