//! Drive the real gateway (`contextstream-mcp http`) against a fake API: whose
//! run a caller's report lands on, and which tool calls cause a sign of life.
//!
//! The gateway runs as the hosted one does: it asks every request for a
//! credential, it holds no secret to verify a bearer token with (the API
//! verifies it), and it keeps runs alive in one workspace
//! (`CONTEXTSTREAM_RUN_PROGRESS_SIGNS_WORKSPACES`).
//!
//! - Two callers it cannot name itself (bearer tokens) that present the same
//!   session id are two sessions: their reports land on two runs, under two
//!   agents, and each one's tool calls signal its own run.
//! - A caller whose token was replaced (the same subject, a new token) keeps
//!   its run; a token that names no subject is a caller of its own.
//! - A credential the API was never seen to accept causes no request of the
//!   gateway's own: not one read of a run and not one sign, however many
//!   such credentials call. What the called tool itself asks is all there is.
//! - "Accept" is the API saying whose credential it is. An answer of a route
//!   that checks no credential (the version) accepts nothing, so keys nobody
//!   issued and tokens that name another's account cause no request, spend
//!   nothing of that account's signs, and take no place of a real caller.
//! - A refusal (401) ends it, whichever of the client's ways the request
//!   went.
//! - Another caller on one's session id cannot rename one's agent.
//! - A request with no credential is refused and causes nothing.
//!
//! Every gateway gets its own HOME; the API is a listener on localhost.
#![cfg(unix)]

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const REPORT: &str = "/api/v1/run-progress/report";
/// A read of one run: what a sign of life starts with.
const RUNS: &str = "/api/v1/run-progress/runs/";
/// What a report that names no run asks first: the workspace's open runs.
const OPEN_RUNS: &str = "/api/v1/run-progress/runs?workspace_id=11111111-1111-4111-8111-111111111111&active=true&limit=100";
/// What `help(action="version")` itself asks the API.
const VERSION: &str = "/api/v1/mcp/version";
const ME: &str = "/api/v1/auth/me";

/// What the fake API was asked: the request line's method and path, the
/// credential the request carried, and its body.
#[derive(Clone, Debug)]
struct Asked {
    method: String,
    path: String,
    credential: String,
    body: Value,
}

/// A stand-in for the ContextStream API. It refuses (401) every credential
/// that starts with `refused-` or was revoked ([`FakeApi::revoke`]) and
/// accepts any other, answers a report and a read of a run the way the API
/// does, and keeps what it was asked. As the API does, it answers what needs
/// no credential ([`VERSION`]) with success whatever credential came with
/// it, and says who a credential belongs to only on [`ME`].
#[derive(Clone)]
struct FakeApi {
    port: u16,
    asked: Arc<Mutex<Vec<Asked>>>,
    revoked: Arc<Mutex<HashSet<String>>>,
}

impl FakeApi {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = Self {
            port: listener.local_addr().unwrap().port(),
            asked: Arc::new(Mutex::new(Vec::new())),
            revoked: Arc::new(Mutex::new(HashSet::new())),
        };
        let (asked, revoked) = (api.asked.clone(), api.revoked.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (asked, revoked) = (asked.clone(), revoked.clone());
                std::thread::spawn(move || serve(stream, &asked, &revoked));
            }
        });
        api
    }

    /// From now on the API refuses `credential` (401), as it does a key that
    /// was deleted or a token that was signed out.
    fn revoke(&self, credential: &str) {
        self.revoked.lock().unwrap().insert(credential.to_string());
    }

    /// The reads of one run the API received under `credential`.
    fn run_reads_under(&self, credential: &str) -> usize {
        self.run_reads()
            .iter()
            .filter(|asked| asked.credential == credential)
            .count()
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }

    /// The reports the API received, oldest first.
    fn reports(&self) -> Vec<Asked> {
        self.asked()
            .into_iter()
            .filter(|asked| asked.path.starts_with(REPORT))
            .collect()
    }

    /// The reads of one run the API received (a sign of life starts with one).
    fn run_reads(&self) -> Vec<Asked> {
        self.asked()
            .into_iter()
            .filter(|asked| asked.method == "GET" && asked.path.starts_with(RUNS))
            .collect()
    }

    /// The paths the API was asked for, oldest first.
    fn paths(&self) -> Vec<String> {
        self.asked().into_iter().map(|asked| asked.path).collect()
    }

    /// Wait, at most a quarter of a minute, until the API has received
    /// `reports` reports: a sign of life is started and never waited on, so
    /// it arrives when it arrives.
    fn wait_for_reports(&self, reports: usize) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.reports().len() < reports && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wait until the API has been quiet for a moment, so whatever a call
    /// started has arrived.
    fn settle(&self) {
        let mut seen = self.asked().len();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            std::thread::sleep(Duration::from_millis(800));
            let now = self.asked().len();
            if now == seen || Instant::now() >= deadline {
                return;
            }
            seen = now;
        }
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The account a credential belongs to, as the fake API has it: the subject
/// a token names; for a key, what stands before `~` in it (so one account
/// can hold several keys), else the whole key.
fn account_of(credential: &str) -> String {
    let segments: Vec<&str> = credential.split('.').collect();
    if let [_, claims, _] = segments[..] {
        let named = base64url_decode(claims)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|claims| claims["sub"].as_str().map(str::to_string));
        if let Some(subject) = named {
            return subject;
        }
    }
    credential
        .split('~')
        .next()
        .unwrap_or(credential)
        .to_string()
}

/// The user id the fake API answers for an account: one id an account.
fn user_id_of(account: &str) -> String {
    let hash = |seed: u64| {
        account.bytes().fold(seed, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let (high, low) = (hash(0xcbf2_9ce4_8422_2325), hash(0x8422_2325_cbf2_9ce4));
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xffff,
        high & 0xfff,
        low >> 52,
        low & 0xffff_ffff_ffff
    )
}

fn serve(mut stream: TcpStream, asked: &Mutex<Vec<Asked>>, revoked: &Mutex<HashSet<String>>) {
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
    let mut first_line = head.lines().next().unwrap_or_default().split(' ');
    let method = first_line.next().unwrap_or_default().to_string();
    let path = first_line.next().unwrap_or_default().to_string();
    let credential = header(&head, "x-api-key")
        .map(str::to_string)
        .or_else(|| {
            header(&head, "authorization")
                .map(|value| value.trim_start_matches("Bearer ").to_string())
        })
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&request[head_end..]).unwrap_or(Value::Null);
    // The version is public: it is answered without a look at the credential.
    let refused = !path.starts_with(VERSION)
        && (credential.starts_with("refused-") || revoked.lock().unwrap().contains(&credential));
    let now = "2099-01-01T00:00:00Z";
    let answer = if refused {
        json!({"error": "unauthorized"})
    } else if path.starts_with(REPORT) {
        json!({"success": true, "data": {
            "run": {
                "run_id": body["run_id"].as_str().unwrap_or("44444444-4444-4444-8444-444444444444"),
                "workspace_id": body["workspace_id"],
                "state": "working",
                "liveness": "live",
                "agent": body["agent"],
            },
            "written": [],
        }})
    } else if method == "GET" && path.starts_with(RUNS) {
        // The run that was asked for, at work, reported on just now.
        let run_id = path[RUNS.len()..].split('?').next().unwrap_or_default();
        json!({"success": true, "data": {"run": {
            "run_id": run_id,
            "workspace_id": WORKSPACE,
            "state": "working",
            "liveness": "live",
            "last_event_at": now,
            "agent": {"id": "claude-code:0badc0de", "name": "Claude Code 0bad", "runtime": "mcp", "client": "claude-code"},
        }}})
    } else if path.starts_with(ME) {
        json!({"success": true, "data": {
            "id": user_id_of(&account_of(&credential)),
            "email": "qa@example.com",
            "created_at": "2026-01-01T00:00:00Z",
        }})
    } else {
        json!({"success": true, "data": {}})
    };
    asked.lock().unwrap().push(Asked {
        method,
        path,
        credential,
        body,
    });
    let text = answer.to_string();
    let status = if refused {
        "401 Unauthorized"
    } else {
        "200 OK"
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        text.len(),
        text
    );
}

/// How a caller presents its credential.
#[derive(Clone, Copy)]
enum Credential<'a> {
    None,
    ApiKey(&'a str),
    Bearer(&'a str),
}

/// A gateway process, as hosted: every request needs a credential, bearer
/// tokens are the API's to verify, and signs of life are on for [`WORKSPACE`].
struct Gateway {
    port: u16,
    process: Child,
    _home: tempfile::TempDir,
}

impl Gateway {
    fn start(api: &FakeApi) -> Self {
        Self::start_with(
            api,
            &[("CONTEXTSTREAM_RUN_PROGRESS_SIGNS_WORKSPACES", WORKSPACE)],
        )
    }

    fn start_with(api: &FakeApi, env: &[(&str, &str)]) -> Self {
        // A free port is found by binding it and letting go, and the gateway
        // binds it a moment later. One gateway starts at a time, so no two
        // tests of this process are handed the same port; a gateway that
        // lost its port to another process exits, and is started again.
        static STARTING: Mutex<()> = Mutex::new(());
        let _starting = STARTING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for _ in 0..5 {
            if let Some(gateway) = Self::try_start(api, env) {
                return gateway;
            }
        }
        panic!("the gateway did not start");
    }

    fn try_start(api: &FakeApi, env: &[(&str, &str)]) -> Option<Self> {
        let home = tempfile::tempdir().unwrap();
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        command
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
            .env("MCP_MAX_CONCURRENT_REQUESTS", "32")
            .current_dir(home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut gateway = Self {
            port,
            process: command.spawn().unwrap(),
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

    /// One request to `/mcp`: the HTTP status of the answer.
    fn post(&self, credential: Credential, headers: &[(&str, &str)], body: &Value) -> u16 {
        self.post_to("/mcp", credential, headers, body)
    }

    /// One request to the endpoint `endpoint`: the HTTP status of the answer.
    fn post_to(
        &self,
        endpoint: &str,
        credential: Credential,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> u16 {
        let text = body.to_string();
        let mut request = format!(
            "POST {endpoint} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.port,
            text.len()
        );
        match credential {
            Credential::None => {}
            Credential::ApiKey(key) => request.push_str(&format!("X-API-Key: {key}\r\n")),
            Credential::Bearer(token) => {
                request.push_str(&format!("Authorization: Bearer {token}\r\n"));
            }
        }
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(&text);
        // The gateway refuses a request without reading its body, and may
        // close the connection before the answer is read here: ask again.
        for _ in 0..20 {
            let Ok(mut stream) = TcpStream::connect(("127.0.0.1", self.port)) else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let _ = stream.write_all(request.as_bytes());
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer);
            let status = String::from_utf8_lossy(&answer)
                .split(' ')
                .nth(1)
                .and_then(|status| status.parse().ok());
            if let Some(status) = status {
                return status;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the gateway did not answer");
    }

    /// `initialize` on the session-id contract.
    fn initialize(&self, credential: Credential) -> u16 {
        self.post(
            credential,
            &[],
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "claude-code", "version": "1"},
            }}),
        )
    }

    /// A tool call on the session-id contract, in the session `session_id`.
    fn call(&self, credential: Credential, session_id: &str, tool: &str, arguments: Value) -> u16 {
        self.post(
            credential,
            &[("Mcp-Session-Id", session_id)],
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments}}),
        )
    }

    /// A tool call on the stateless contract, from the connection `token`.
    fn stateless_call(
        &self,
        credential: Credential,
        token: &str,
        tool: &str,
        arguments: Value,
    ) -> u16 {
        self.post(
            credential,
            &[
                ("MCP-Protocol-Version", "2026-07-28"),
                ("Mcp-Method", "tools/call"),
                ("Mcp-Name", tool),
                ("X-ContextStream-Client-Session", token),
            ],
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": tool,
                "arguments": arguments,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {"name": "claude-code", "version": "1"},
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            }}),
        )
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut bits, mut held) = (0_u32, 0_u32);
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|letter| *letter == byte)? as u32;
        bits = bits << 6 | value;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
            bits &= (1 << held) - 1;
        }
    }
    Some(out)
}

fn base64url(bytes: &[u8]) -> String {
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

/// A bearer token shaped as the API's are (three segments, RS256): `claims`
/// and a made-up signature. Nothing here can verify it, as on the hosted
/// gateway.
fn bearer(claims: Value, signature: &str) -> String {
    format!(
        "{}.{}.{signature}",
        base64url(br#"{"alg":"RS256","typ":"JWT"}"#),
        base64url(claims.to_string().as_bytes())
    )
}

fn a_step(step: &str) -> Value {
    json!({"workspace_id": WORKSPACE, "step": step})
}

/// A tool call in the workspace the gateway signs. The tool asks the API one
/// thing of its own ([`VERSION`]).
fn version() -> Value {
    json!({"action": "version", "workspace_id": WORKSPACE})
}

/// A tool call in the workspace the gateway signs that asks the API nothing
/// itself: whatever the API is asked then is the gateway's own doing.
fn local() -> Value {
    json!({"action": "tools", "workspace_id": WORKSPACE})
}

/// The version, asked with no workspace named: the API answers it without a
/// look at the credential.
fn public_version() -> Value {
    json!({"action": "version"})
}

fn run_key(report: &Asked) -> String {
    report.body["source_run_id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn agent_id(report: &Asked) -> String {
    report.body["agent"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The second reading's MUST 1. Two people sign in (bearer tokens the gateway
/// cannot verify) and present the same session id, or the same connection
/// token: they are two sessions with a run and an agent each, and a tool call
/// of one signals that one's run.
#[test]
fn two_callers_the_gateway_cannot_name_never_share_a_run() {
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    let ann = bearer(json!({"sub": "user-ann"}), "signature-a");
    let bob = bearer(json!({"sub": "user-bob"}), "signature-b");

    // The session-id contract: the id is the client's to send.
    gateway.call(Credential::Bearer(&ann), "sid-42", "progress", a_step("A"));
    gateway.call(Credential::Bearer(&bob), "sid-42", "progress", a_step("B"));
    let reports = api.reports();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_eq!(
        (
            reports[0].credential.as_str(),
            reports[1].credential.as_str()
        ),
        (ann.as_str(), bob.as_str())
    );
    let (ann_key, bob_key) = (run_key(&reports[0]), run_key(&reports[1]));
    assert!(
        ann_key.starts_with("s-") && bob_key.starts_with("s-"),
        "{ann_key} {bob_key}"
    );
    assert_ne!(
        ann_key, bob_key,
        "two callers with one session id share a run"
    );
    assert_ne!(
        agent_id(&reports[0]),
        agent_id(&reports[1]),
        "two callers with one session id are one agent"
    );

    // The stateless contract: the connection token is the client's to send.
    let token = "a".repeat(32);
    gateway.stateless_call(Credential::Bearer(&ann), &token, "progress", a_step("A"));
    gateway.stateless_call(Credential::Bearer(&bob), &token, "progress", a_step("B"));
    let reports = api.reports();
    assert_eq!(reports.len(), 4, "{reports:?}");
    let (ann_stateless, bob_stateless) = (run_key(&reports[2]), run_key(&reports[3]));
    assert!(
        ann_stateless.starts_with("s-") && bob_stateless.starts_with("s-"),
        "a signed-in caller that names its connection is one session: {ann_stateless} {bob_stateless}"
    );
    assert_ne!(ann_stateless, bob_stateless);

    // A tool call of Ann's session, on either contract, signals a run, under
    // Ann's token, and Bob's token is never used for it.
    api.settle();
    let before = api.asked().len();
    gateway.call(Credential::Bearer(&ann), "sid-42", "help", version());
    gateway.stateless_call(Credential::Bearer(&ann), &token, "help", version());
    api.wait_for_reports(4 + 2);
    api.settle();
    let after: Vec<Asked> = api.asked().into_iter().skip(before).collect();
    let reads: Vec<&Asked> = after
        .iter()
        .filter(|asked| asked.method == "GET" && asked.path.starts_with(RUNS))
        .collect();
    assert_eq!(
        reads.len(),
        2,
        "each session of a signed-in caller is signalled: {after:?}"
    );
    assert!(
        after.iter().all(|asked| asked.credential == ann),
        "{after:?}"
    );
    assert_ne!(reads[0].path, reads[1].path, "two sessions, two runs");
    let signs: Vec<&Asked> = after
        .iter()
        .filter(|asked| asked.path.starts_with(REPORT))
        .collect();
    assert_eq!(signs.len(), 2, "{after:?}");
    for sign in signs {
        // Who and which run, and nothing about the work.
        let mut said: Vec<&String> = sign.body.as_object().unwrap().keys().collect();
        said.sort();
        assert_eq!(said, ["agent", "run_id", "workspace_id"], "{sign:?}");
    }
}

/// What a replaced token does to a run. A client replaces its access token
/// every hour: the token names the same subject, and the session keeps its
/// run. A token that names no subject is a caller of its own, so a new one
/// starts a new run and is never another caller's.
#[test]
fn a_replaced_token_keeps_the_run_when_it_names_the_same_subject() {
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    let first = bearer(
        json!({"sub": "user-ann", "jti": "1", "exp": 4_000_000_000_u64}),
        "signature-1",
    );
    let second = bearer(
        json!({"sub": "user-ann", "jti": "2", "exp": 4_000_003_600_u64}),
        "signature-2",
    );
    gateway.call(
        Credential::Bearer(&first),
        "sid-7",
        "progress",
        a_step("Before"),
    );
    gateway.call(
        Credential::Bearer(&second),
        "sid-7",
        "progress",
        a_step("After"),
    );
    let reports = api.reports();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_eq!(
        run_key(&reports[0]),
        run_key(&reports[1]),
        "a replaced token of one subject starts another run"
    );
    assert_eq!(agent_id(&reports[0]), agent_id(&reports[1]));

    // No subject to read: the token is the caller.
    let opaque_1 = "opaque.token.one";
    let opaque_2 = "opaque.token.two";
    gateway.call(
        Credential::Bearer(opaque_1),
        "sid-7",
        "progress",
        a_step("One"),
    );
    gateway.call(
        Credential::Bearer(opaque_2),
        "sid-7",
        "progress",
        a_step("Two"),
    );
    gateway.call(
        Credential::Bearer(opaque_1),
        "sid-7",
        "progress",
        a_step("One again"),
    );
    let reports = api.reports();
    assert_eq!(reports.len(), 5, "{reports:?}");
    let keys: Vec<String> = reports.iter().map(run_key).collect();
    assert_ne!(keys[2], keys[3], "two tokens that name nobody share a run");
    assert_eq!(keys[2], keys[4], "one token is one caller");
    assert!(keys[2] != keys[0] && keys[3] != keys[0], "{keys:?}");
    // Nothing of a token or a subject is in a key or an agent.
    for report in &reports {
        let sent = report.body.to_string();
        for secret in ["user-ann", "signature", "opaque", "token"] {
            assert!(!sent.contains(secret), "{secret} in {sent}");
        }
    }
}

/// A token that names another caller's subject and that the API refuses
/// writes nothing and signals nothing: the gateway has no authority of its
/// own, and the API never accepted the token.
#[test]
fn a_refused_token_that_names_anothers_subject_writes_and_signals_nothing() {
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    let ann = bearer(json!({"sub": "user-ann"}), "signature-a");
    gateway.call(Credential::Bearer(&ann), "sid-42", "progress", a_step("A"));
    api.settle();
    let before = api.asked().len();
    // The fake API refuses this one, as the API refuses a token it did not
    // issue.
    let forged = format!("refused-{}", bearer(json!({"sub": "user-ann"}), "forged"));
    gateway.call(
        Credential::Bearer(&forged),
        "sid-42",
        "progress",
        a_step("X"),
    );
    for _ in 0..5 {
        gateway.call(Credential::Bearer(&forged), "sid-42", "help", version());
    }
    api.settle();
    let after: Vec<Asked> = api.asked().into_iter().skip(before).collect();
    // What the tools themselves ask reached the API under the forged token
    // and was refused there: the report (and the read of open runs before
    // it), and the five calls' own question. Nothing else was asked: no
    // read of a run and no sign, under any credential.
    let paths: Vec<&str> = after.iter().map(|asked| asked.path.as_str()).collect();
    assert_eq!(
        paths,
        [OPEN_RUNS, REPORT, VERSION, VERSION, VERSION, VERSION, VERSION],
        "{after:?}"
    );
    assert!(
        after.iter().all(|asked| asked.credential == forged),
        "{after:?}"
    );
}

/// The second reading's MUST 3, first half. A credential the API was never
/// seen to accept causes no request of the gateway's own, however many of
/// them call: not a read of a run, not a sign, not a check of the
/// credential. The API is asked what the called tool itself asks, once a
/// call, as with the setting off. A key the API accepted is signalled.
#[test]
fn a_credential_the_api_never_accepted_causes_no_request_of_the_gateways_own() {
    const BOGUS: usize = 40;
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);

    // Keys nobody issued, each in a session of its own.
    for index in 0..BOGUS {
        let status = gateway.call(
            Credential::ApiKey(&format!("refused-{index}")),
            &format!("bogus-{index}"),
            "help",
            version(),
        );
        assert_eq!(status, 200, "a tool call is answered as before");
    }
    api.settle();
    assert_eq!(
        api.paths(),
        vec![VERSION; BOGUS],
        "keys the API never accepted caused requests of the gateway's own"
    );

    // A key the API accepts: `initialize` asks the API once, and from then on
    // the key's sessions are signalled: a read of the run and a sign each.
    assert_eq!(gateway.initialize(Credential::ApiKey("a-good-key")), 200);
    for index in 0..3 {
        gateway.call(
            Credential::ApiKey("a-good-key"),
            &format!("good-{index}"),
            "help",
            version(),
        );
    }
    api.wait_for_reports(3);
    api.settle();
    let good: Vec<Asked> = api.asked().into_iter().skip(BOGUS).collect();
    assert!(
        good.iter().all(|asked| asked.credential == "a-good-key"),
        "{good:?}"
    );
    assert_eq!(api.run_reads().len(), 3, "{good:?}");
    assert_eq!(api.reports().len(), 3, "{good:?}");
    assert_eq!(good.len(), 1 + 3 * 3, "{good:?}");

    // The bogus keys again, now in the good key's sessions: still nothing
    // but the tool's own question.
    let before = api.asked().len();
    for index in 0..BOGUS {
        gateway.call(
            Credential::ApiKey(&format!("refused-{index}")),
            &format!("good-{}", index % 3),
            "help",
            version(),
        );
    }
    api.settle();
    let after: Vec<String> = api.paths().into_iter().skip(before).collect();
    assert_eq!(after, vec![VERSION; BOGUS]);
}

/// A request with no credential at all is refused before anything else: no
/// run is derived for it and the API is not asked.
#[test]
fn a_request_without_a_credential_is_refused_and_asks_the_api_nothing() {
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    assert_eq!(
        gateway.call(Credential::None, "sid-42", "progress", a_step("A")),
        401
    );
    assert_eq!(
        gateway.call(Credential::None, "sid-42", "help", version()),
        401
    );
    assert_eq!(
        gateway.stateless_call(Credential::None, &"a".repeat(32), "help", version()),
        401
    );
    api.settle();
    assert_eq!(api.asked().len(), 0, "{:?}", api.asked());
}

/// As shipped (the setting unset), and for a workspace the setting does not
/// name, a tool call causes no request of the gateway's own: only what the
/// tool itself asks.
#[test]
fn nothing_is_asked_for_a_workspace_the_setting_does_not_name() {
    let other = "22222222-2222-4222-8222-222222222222";
    for env in [
        vec![],
        vec![("CONTEXTSTREAM_RUN_PROGRESS_SIGNS_WORKSPACES", other)],
    ] {
        let api = FakeApi::start();
        let gateway = Gateway::start_with(&api, &env);
        let ann = bearer(json!({"sub": "user-ann"}), "signature-a");
        for credential in [Credential::ApiKey("a-good-key"), Credential::Bearer(&ann)] {
            assert_eq!(gateway.initialize(credential), 200);
            gateway.call(credential, "sid-1", "progress", a_step("A"));
            for _ in 0..3 {
                gateway.call(credential, "sid-1", "help", version());
            }
        }
        api.settle();
        // For each caller: the check of the credential at `initialize`, the
        // report (and the read of open runs before it), and each call's own
        // question. Nothing else: no read of a run, no sign.
        let one_caller = [ME, OPEN_RUNS, REPORT, VERSION, VERSION, VERSION];
        assert_eq!(api.paths(), [one_caller, one_caller].concat(), "{env:?}");
    }
}

/// The delta reading's MUST 1, first part. The API answers what needs no
/// credential with success whatever credential came with it. That answer
/// says nothing about the credential: a key nobody issued and a token nobody
/// issued stay unaccepted, and cause no request of the gateway's own.
#[test]
fn an_answer_that_checked_no_credential_accepts_none() {
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    let forged = format!("refused-{}", bearer(json!({"sub": "user-ann"}), "forged"));
    for credential in [
        Credential::ApiKey("refused-probe"),
        Credential::Bearer(&forged),
    ] {
        assert_eq!(
            gateway.call(credential, "s-probe", "help", public_version()),
            200
        );
        for _ in 0..3 {
            assert_eq!(gateway.call(credential, "s-probe", "help", local()), 200);
        }
    }
    api.settle();
    let paths = api.paths();
    assert!(
        !paths.is_empty() && paths.iter().all(|path| path == VERSION),
        "a success of a route that checks no credential accepted one: {paths:?}"
    );
}

/// The delta reading's MUST 1, second part. Sixty tokens nobody issued name
/// Ann's account, one of them on Ann's own session id. None is accepted, so
/// none reads a run, and none spends a sign of Ann's: her own session's next
/// tool call is signalled.
#[test]
fn tokens_the_api_refuses_spend_no_sign_of_the_account_they_name() {
    const FORGED: usize = 60;
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    let ann = bearer(json!({"sub": "user-ann", "jti": "real"}), "signature-a");
    assert_eq!(gateway.initialize(Credential::Bearer(&ann)), 200);
    gateway.call(Credential::Bearer(&ann), "ann-sid", "progress", a_step("A"));
    api.settle();

    for index in 0..FORGED {
        let forged = format!(
            "refused-{}",
            bearer(json!({"sub": "user-ann", "n": index}), "forged")
        );
        let session = if index == 0 {
            "ann-sid".to_string()
        } else {
            format!("forged-{index}")
        };
        gateway.call(
            Credential::Bearer(&forged),
            &session,
            "help",
            public_version(),
        );
        gateway.call(Credential::Bearer(&forged), &session, "help", local());
    }
    api.settle();
    let forged_reads: Vec<Asked> = api
        .run_reads()
        .into_iter()
        .filter(|asked| asked.credential != ann)
        .collect();
    assert!(
        forged_reads.is_empty(),
        "tokens the API refuses read a run: {} reads",
        forged_reads.len()
    );
    assert_eq!(api.run_reads_under(&ann), 0);

    gateway.call(Credential::Bearer(&ann), "ann-sid", "help", local());
    api.wait_for_reports(2);
    api.settle();
    assert_eq!(
        api.run_reads_under(&ann),
        1,
        "tokens the API refuses spent the signs of the account they name"
    );
    assert_eq!(api.reports().len(), 2, "Ann's report, and the sign");
}

/// The delta reading's MUST 1, third part. More keys nobody issued than the
/// gateway remembers credentials call what needs no credential. None takes a
/// place: a caller the API accepts afterwards is signalled.
#[test]
fn keys_nobody_issued_take_no_place_of_a_caller_the_api_accepts() {
    const BOGUS: usize = 10_050;
    const AT_ONCE: usize = 16;
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    std::thread::scope(|scope| {
        for worker in 0..AT_ONCE {
            let gateway = &gateway;
            scope.spawn(move || {
                for index in (worker..BOGUS).step_by(AT_ONCE) {
                    gateway.call(
                        Credential::ApiKey(&format!("refused-fill-{index}")),
                        "fill",
                        "help",
                        public_version(),
                    );
                }
            });
        }
    });
    api.settle();
    assert!(api.run_reads().is_empty());

    assert_eq!(gateway.initialize(Credential::ApiKey("a-new-key")), 200);
    gateway.call(Credential::ApiKey("a-new-key"), "new-sid", "help", local());
    api.wait_for_reports(1);
    api.settle();
    assert_eq!(
        api.run_reads_under("a-new-key"),
        1,
        "keys nobody issued took the place of a caller the API accepts"
    );
}

/// The delta reading's MUST 2. The client asks the API in three ways (an
/// answer it reads as JSON, one it reads as text, one it passes on as it
/// is). A refusal seen on any of them ends the credential's acceptance: no
/// further read of a run goes out under it.
#[test]
fn a_refusal_ends_the_acceptance_whichever_way_the_request_went() {
    let capsule = "33333333-3333-4333-8333-333333333333";
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    let keys = ["key-json", "key-text", "key-raw"];
    for key in keys {
        assert_eq!(gateway.initialize(Credential::ApiKey(key)), 200);
        // Accepted: a session of the key is signalled.
        gateway.call(
            Credential::ApiKey(key),
            &format!("{key}-1"),
            "help",
            local(),
        );
    }
    api.wait_for_reports(keys.len());
    api.settle();
    for key in keys {
        assert_eq!(api.run_reads_under(key), 1, "{key}");
        api.revoke(key);
    }
    let before = api.asked().len();

    // Each key is refused once, on one of the three ways.
    gateway.call(
        Credential::ApiKey("key-json"),
        "key-json-2",
        "help",
        json!({"action": "auth"}),
    );
    gateway.call(
        Credential::ApiKey("key-text"),
        "key-text-2",
        "capsule",
        json!({"action": "open", "capsule_id": capsule, "format": "markdown"}),
    );
    gateway.post_to(
        "/coach",
        Credential::ApiKey("key-raw"),
        &[("Mcp-Session-Id", "key-raw-2")],
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "workflow", "arguments": {"action": "capabilities"}}}),
    );
    api.settle();
    let refused: Vec<(String, String)> = api
        .asked()
        .into_iter()
        .skip(before)
        .map(|asked| (asked.credential, asked.path))
        .collect();
    let refused_on = |key: &str, path: &str| {
        refused
            .iter()
            .any(|(credential, asked)| credential == key && asked.contains(path))
    };
    assert!(
        refused_on("key-json", ME)
            && refused_on("key-text", "/context.md")
            && refused_on("key-raw", "/coach/capabilities"),
        "each key was to be refused on its way: {refused:?}"
    );

    // No further read of a run, in any session of a refused key.
    for key in keys {
        gateway.call(
            Credential::ApiKey(key),
            &format!("{key}-3"),
            "help",
            local(),
        );
    }
    api.settle();
    let reads: Vec<(&str, usize)> = keys
        .iter()
        .map(|key| (*key, api.run_reads_under(key)))
        .collect();
    assert_eq!(
        reads,
        [("key-json", 1), ("key-text", 1), ("key-raw", 1)],
        "a refused credential went on reading runs"
    );
}

/// The delta reading's MUST 3. Two people who are signed in present the same
/// session id. One names a folder for the session; the other's agent stays
/// what it was: its own id and name, on its own run.
#[test]
fn another_caller_on_ones_session_id_cannot_rename_ones_agent() {
    let api = FakeApi::start();
    let gateway = Gateway::start(&api);
    let ann = bearer(json!({"sub": "user-ann"}), "signature-a");
    let bob = bearer(json!({"sub": "user-bob"}), "signature-b");
    gateway.call(
        Credential::Bearer(&ann),
        "sid-42",
        "progress",
        a_step("Before"),
    );
    gateway.call(
        Credential::Bearer(&bob),
        "sid-42",
        "init",
        json!({"folder_path": "/work/agent-hijacked"}),
    );
    gateway.call(
        Credential::Bearer(&ann),
        "sid-42",
        "progress",
        a_step("After"),
    );
    let reports: Vec<Asked> = api
        .reports()
        .into_iter()
        .filter(|report| report.credential == ann)
        .collect();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(!agent_id(&reports[0]).is_empty(), "{reports:?}");
    assert_eq!(
        reports[0].body["agent"], reports[1].body["agent"],
        "another caller renamed the agent"
    );
    assert_eq!(run_key(&reports[0]), run_key(&reports[1]));
}
