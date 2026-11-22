//! Verification gates: policy lookup, receipt verification checks, and a
//! short-TTL disk cache so hooks avoid repeated policy round-trips.
//!
//! Endpoints:
//! - `GET /api/v1/mcp/hosted-tools/gate-policy`
//! - `GET /api/v1/mcp/hosted-tools/receipts/check`
//!
//! Every lookup is fail-open on 404, non-2xx, network errors, timeouts, or
//! malformed payloads (`Ok(None)`), so rolling deploys or offline work never
//! break local commits, pushes, or completions.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::client::ContextStreamClient;
use crate::command_hints;
use mcp_types::Result;

/// API path for workspace/project verification gate policy.
pub const GATE_POLICY_PATH: &str = "/mcp/hosted-tools/gate-policy";
/// API path for checking verification receipts for a tree + gate.
pub const RECEIPTS_CHECK_PATH: &str = "/mcp/hosted-tools/receipts/check";

/// Disk cache file prefix under `~/.contextstream/`.
pub const GATE_POLICY_CACHE_FILE: &str = "hosted-gate-policy";
/// Short TTL for cached gate-policy reads (45 seconds).
pub const GATE_POLICY_CACHE_TTL: Duration = Duration::from_secs(45);

const CACHE_VERSION: u32 = 1;

/// Workspace or project verification gate policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct GatePolicy {
    pub require_verified_tree_before_push: bool,
    pub require_verified_tree_before_pr: bool,
    pub require_verified_tree_before_complete: bool,
    pub required_command_families: Vec<String>,
    pub receipt_ttl_secs: i32,
    pub updated_at: Option<String>,
}

impl GatePolicy {
    /// Whether the named gate (`push`, `pr`, `complete`) is enabled in this policy.
    pub fn is_gate_enabled(&self, gate: &str) -> bool {
        match gate {
            "push" => self.require_verified_tree_before_push,
            "pr" => self.require_verified_tree_before_pr,
            "complete" => self.require_verified_tree_before_complete,
            _ => false,
        }
    }
}

/// Response payload from `GET /api/v1/mcp/hosted-tools/gate-policy`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct GatePolicyResponse {
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub inherited: bool,
    pub policy: GatePolicy,
}

/// Response payload from `GET /api/v1/mcp/hosted-tools/receipts/check`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ReceiptCheckResult {
    pub allowed: bool,
    pub gate: String,
    pub gate_enabled: bool,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub repo_key: String,
    pub target_tree: String,
    pub ttl_secs: i32,
    pub missing_command_families: Vec<String>,
    pub required_command_families: Vec<String>,
    pub policy: Option<GatePolicy>,
    pub receipts: Vec<Value>,
}

/// On-disk cached gate-policy envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GatePolicyCache {
    pub version: u32,
    pub identity: String,
    pub api_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub written_at: u64,
    pub response: GatePolicyResponse,
}

impl GatePolicyCache {
    pub fn new(
        identity: &str,
        api_url: &str,
        workspace_id: Option<&str>,
        project_id: Option<&str>,
        response: GatePolicyResponse,
    ) -> Self {
        Self {
            version: CACHE_VERSION,
            identity: identity.to_string(),
            api_url: normalized_url(api_url),
            workspace_id: clean_opt(workspace_id),
            project_id: clean_opt(project_id),
            written_at: now_secs(),
            response,
        }
    }

    pub fn is_fresh(&self, ttl: Duration) -> bool {
        Duration::from_secs(now_secs().saturating_sub(self.written_at)) < ttl
    }
}

fn clean_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn normalized_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Unwrap ContextStream's `{success, data}` `ApiResponse` envelope, matching
/// `HostedToolsAnswer::from_body`.
fn unwrap_data_envelope(body: Value) -> Value {
    match &body {
        Value::Object(map) if map.contains_key("success") && map.contains_key("data") => {
            map.get("data").cloned().unwrap_or(Value::Null)
        }
        Value::Object(map)
            if map.get("data").is_some_and(Value::is_object)
                && !map.contains_key("allowed")
                && !map.contains_key("policy") =>
        {
            map.get("data").cloned().unwrap_or(Value::Null)
        }
        other => other.clone(),
    }
}

/// Path of the cached gate-policy file under `~/.contextstream/`.
pub fn cache_path(
    identity: &str,
    api_url: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> Option<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(identity.as_bytes());
    hasher.update(b"\0");
    hasher.update(normalized_url(api_url).as_bytes());
    hasher.update(b"\0");
    hasher.update(clean_opt(workspace_id).unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    hasher.update(clean_opt(project_id).unwrap_or_default().as_bytes());
    let digest = hex::encode(hasher.finalize());
    dirs::home_dir().map(|home| {
        home.join(".contextstream")
            .join(format!("{GATE_POLICY_CACHE_FILE}-{}.json", &digest[..32]))
    })
}

/// Read a fresh cached gate-policy entry from `path`, ignoring symlinks and
/// oversized/stale/mismatched files.
pub fn read_from(
    path: &Path,
    identity: &str,
    api_url: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    ttl: Duration,
) -> Option<GatePolicyCache> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 1 << 20 {
        return None;
    }
    let cache: GatePolicyCache = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (cache.version == CACHE_VERSION
        && cache.identity == identity
        && cache.api_url == normalized_url(api_url)
        && cache.workspace_id == clean_opt(workspace_id)
        && cache.project_id == clean_opt(project_id)
        && cache.is_fresh(ttl))
    .then_some(cache)
}

/// Atomically write `cache` to `path` via temp file + rename. Refuses symlinks.
pub fn write_to(path: &Path, cache: &GatePolicyCache) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the gate policy cache path is not a regular file",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no folder"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(
        &mut temporary,
        serde_json::to_string(cache)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            .as_bytes(),
    )?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Read the cached gate policy for this caller/scope when fresh.
pub fn read_cached(
    identity: &str,
    api_url: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> Option<GatePolicyResponse> {
    let path = cache_path(identity, api_url, workspace_id, project_id)?;
    read_from(
        &path,
        identity,
        api_url,
        workspace_id,
        project_id,
        GATE_POLICY_CACHE_TTL,
    )
    .map(|entry| entry.response)
}

/// Write `response` to the default gate-policy cache file for this caller/scope.
pub fn write_cached(
    identity: &str,
    api_url: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    response: &GatePolicyResponse,
) -> std::io::Result<()> {
    let path = cache_path(identity, api_url, workspace_id, project_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home folder"))?;
    let entry = GatePolicyCache::new(
        identity,
        api_url,
        workspace_id,
        project_id,
        response.clone(),
    );
    write_to(&path, &entry)
}

fn append_query_param(params: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = clean_opt(value) {
        params.push(format!("{key}={}", urlencoding::encode(&value)));
    }
}

impl ContextStreamClient {
    /// Fetch the effective verification gate policy for `workspace_id` / `project_id`.
    /// Fails open (`Ok(None)`) on 404, non-2xx, timeout, or network error.
    pub async fn get_gate_policy(
        &self,
        workspace_id: Option<&str>,
        project_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Option<GatePolicyResponse>> {
        let mut params = Vec::new();
        append_query_param(&mut params, "workspace_id", workspace_id);
        append_query_param(&mut params, "project_id", project_id);
        let path = if params.is_empty() {
            GATE_POLICY_PATH.to_string()
        } else {
            format!("{GATE_POLICY_PATH}?{}", params.join("&"))
        };
        let raw = match self.send_raw_json("GET", &path, None, timeout).await {
            Ok(raw) => raw,
            Err(_) => return Ok(None),
        };
        if !(200..300).contains(&raw.status) {
            return Ok(None);
        }
        if raw.body.get("success").and_then(Value::as_bool) == Some(false) {
            return Ok(None);
        }
        let data = unwrap_data_envelope(raw.body);
        Ok(serde_json::from_value::<GatePolicyResponse>(data).ok())
    }

    /// Check whether `target_tree` has valid verification receipts for `gate`
    /// (`"push"`, `"pr"`, or `"complete"`).
    /// Fails open (`Ok(None)`) on 404, non-2xx, timeout, or network error.
    pub async fn check_receipts(
        &self,
        target_tree: &str,
        gate: &str,
        workspace_id: Option<&str>,
        project_id: Option<&str>,
        repo_key: Option<&str>,
        timeout: Duration,
    ) -> Result<Option<ReceiptCheckResult>> {
        let mut params = vec![
            format!("target_tree={}", urlencoding::encode(target_tree.trim())),
            format!("gate={}", urlencoding::encode(gate.trim())),
        ];
        append_query_param(&mut params, "workspace_id", workspace_id);
        append_query_param(&mut params, "project_id", project_id);
        append_query_param(&mut params, "repo_key", repo_key);
        let path = format!("{RECEIPTS_CHECK_PATH}?{}", params.join("&"));
        let raw = match self.send_raw_json("GET", &path, None, timeout).await {
            Ok(raw) => raw,
            Err(_) => return Ok(None),
        };
        if !(200..300).contains(&raw.status) {
            return Ok(None);
        }
        if raw.body.get("success").and_then(Value::as_bool) == Some(false) {
            return Ok(None);
        }
        let data = unwrap_data_envelope(raw.body);
        Ok(serde_json::from_value::<ReceiptCheckResult>(data).ok())
    }
}

/// Resolve the effective verification gate policy using the 45s disk cache,
/// falling back to `get_gate_policy` and caching on success.
/// Bypassed (`None`) when `CONTEXTSTREAM_HOSTED_TOOLS=off`.
pub async fn cached_gate_policy(
    client: &ContextStreamClient,
    identity: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    timeout: Duration,
) -> Option<GatePolicyResponse> {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return None;
    }
    let config = client.config().await;
    let api_url = config.api_url;
    if let Some(cached) = read_cached(identity, &api_url, workspace_id, project_id) {
        return Some(cached);
    }
    let fetched = client
        .get_gate_policy(workspace_id, project_id, timeout)
        .await
        .ok()
        .flatten()?;
    let _ = write_cached(identity, &api_url, workspace_id, project_id, &fetched);
    Some(fetched)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use mcp_types::Config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_once(status: &str, body: &str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
            req
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn get_gate_policy_unwraps_api_response_envelope() {
        let payload = serde_json::json!({
            "success": true,
            "data": {
                "workspace_id": "11111111-1111-1111-1111-111111111111",
                "project_id": "22222222-2222-2222-2222-222222222222",
                "inherited": true,
                "policy": {
                    "require_verified_tree_before_push": true,
                    "require_verified_tree_before_pr": true,
                    "require_verified_tree_before_complete": false,
                    "required_command_families": ["cargo test"],
                    "receipt_ttl_secs": 3600,
                    "updated_at": "2026-11-22T00:00:00Z"
                }
            }
        });
        let (url, req_handle) = mock_once("200 OK", &payload.to_string()).await;
        let client = ContextStreamClient::new(Config {
            api_url: url,
            api_key: Some("k".into()),
            ..Default::default()
        });
        let resp = client
            .get_gate_policy(
                Some("11111111-1111-1111-1111-111111111111"),
                Some("22222222-2222-2222-2222-222222222222"),
                Duration::from_secs(2),
            )
            .await
            .unwrap()
            .expect("policy parsed");
        assert!(resp.inherited);
        assert!(resp.policy.require_verified_tree_before_push);
        assert!(resp.policy.require_verified_tree_before_pr);
        assert!(!resp.policy.require_verified_tree_before_complete);
        assert_eq!(resp.policy.required_command_families, vec!["cargo test"]);
        assert_eq!(resp.policy.receipt_ttl_secs, 3600);
        let req = req_handle.await.unwrap();
        assert!(req.starts_with("GET /api/v1/mcp/hosted-tools/gate-policy?"));
        assert!(req.contains("workspace_id=11111111-1111-1111-1111-111111111111"));
        assert!(req.contains("project_id=22222222-2222-2222-2222-222222222222"));
    }

    #[tokio::test]
    async fn check_receipts_deserializes_locked_wire_contract() {
        let tree = "a".repeat(40);
        let payload = serde_json::json!({
            "success": true,
            "data": {
                "allowed": false,
                "gate": "push",
                "gate_enabled": true,
                "workspace_id": "11111111-1111-1111-1111-111111111111",
                "project_id": null,
                "repo_key": "repo-12345678",
                "target_tree": tree,
                "ttl_secs": 3600,
                "missing_command_families": ["cargo test"],
                "required_command_families": ["cargo test", "cargo clippy"],
                "policy": {
                    "require_verified_tree_before_push": true,
                    "require_verified_tree_before_pr": false,
                    "require_verified_tree_before_complete": false,
                    "required_command_families": ["cargo test", "cargo clippy"],
                    "receipt_ttl_secs": 3600,
                    "updated_at": null
                },
                "receipts": [{"command_family": "cargo clippy"}]
            }
        });
        let (url, req_handle) = mock_once("200 OK", &payload.to_string()).await;
        let client = ContextStreamClient::new(Config {
            api_url: url,
            api_key: Some("k".into()),
            ..Default::default()
        });
        let res = client
            .check_receipts(
                &tree,
                "push",
                Some("11111111-1111-1111-1111-111111111111"),
                None,
                Some("repo-12345678"),
                Duration::from_secs(2),
            )
            .await
            .unwrap()
            .expect("receipt check parsed");
        assert!(!res.allowed);
        assert!(res.gate_enabled);
        assert_eq!(res.gate, "push");
        assert_eq!(res.repo_key, "repo-12345678");
        assert_eq!(res.target_tree, tree);
        assert_eq!(res.ttl_secs, 3600);
        assert_eq!(res.missing_command_families, vec!["cargo test"]);
        assert_eq!(
            res.required_command_families,
            vec!["cargo test", "cargo clippy"]
        );
        assert_eq!(res.receipts.len(), 1);
        assert!(res
            .policy
            .as_ref()
            .is_some_and(|p| p.require_verified_tree_before_push));
        let req = req_handle.await.unwrap();
        assert!(req.starts_with("GET /api/v1/mcp/hosted-tools/receipts/check?"));
        assert!(req.contains("gate=push"));
        assert!(req.contains("repo_key=repo-12345678"));
    }

    #[tokio::test]
    async fn endpoints_fail_open_on_404_500_and_unreachable_server() {
        let (url_404, _) = mock_once("404 Not Found", r#"{"success":false}"#).await;
        let client_404 = ContextStreamClient::new(Config {
            api_url: url_404,
            api_key: Some("k".into()),
            ..Default::default()
        });
        assert!(client_404
            .get_gate_policy(Some("ws"), None, Duration::from_secs(1))
            .await
            .unwrap()
            .is_none());

        let (url_500, _) = mock_once("500 Internal Server Error", "oops").await;
        let client_500 = ContextStreamClient::new(Config {
            api_url: url_500,
            api_key: Some("k".into()),
            ..Default::default()
        });
        assert!(client_500
            .check_receipts(
                &"a".repeat(40),
                "push",
                Some("ws"),
                None,
                None,
                Duration::from_secs(1),
            )
            .await
            .unwrap()
            .is_none());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = listener.local_addr().unwrap();
        drop(listener);
        let offline = ContextStreamClient::new(Config {
            api_url: format!("http://{closed_port}"),
            api_key: Some("k".into()),
            ..Default::default()
        });
        assert!(offline
            .get_gate_policy(Some("ws"), None, Duration::from_millis(200))
            .await
            .unwrap()
            .is_none());
        assert!(offline
            .check_receipts(
                &"a".repeat(40),
                "push",
                Some("ws"),
                None,
                None,
                Duration::from_millis(200),
            )
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn disk_cache_round_trips_respects_ttl_and_rejects_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hosted-gate-policy-test.json");
        let resp = GatePolicyResponse {
            workspace_id: Some("ws-1".into()),
            project_id: Some("proj-1".into()),
            inherited: false,
            policy: GatePolicy {
                require_verified_tree_before_push: true,
                ..Default::default()
            },
        };
        let entry = GatePolicyCache::new(
            "local:abc",
            "https://api.contextstream.io/",
            Some("ws-1"),
            Some("proj-1"),
            resp.clone(),
        );
        write_to(&path, &entry).unwrap();

        let hit = read_from(
            &path,
            "local:abc",
            "https://api.contextstream.io",
            Some("ws-1"),
            Some("proj-1"),
            GATE_POLICY_CACHE_TTL,
        )
        .expect("fresh cache hit");
        assert_eq!(hit.response, resp);

        // Scope mismatch misses cleanly.
        assert!(read_from(
            &path,
            "local:abc",
            "https://api.contextstream.io",
            Some("ws-1"),
            Some("proj-2"),
            GATE_POLICY_CACHE_TTL,
        )
        .is_none());

        // Expired entry misses cleanly.
        let mut stale = entry.clone();
        stale.written_at = stale.written_at.saturating_sub(120);
        write_to(&path, &stale).unwrap();
        assert!(read_from(
            &path,
            "local:abc",
            "https://api.contextstream.io",
            Some("ws-1"),
            Some("proj-1"),
            GATE_POLICY_CACHE_TTL,
        )
        .is_none());

        #[cfg(unix)]
        {
            let target = temp.path().join("real.json");
            std::fs::write(&target, "{}").unwrap();
            let link = temp.path().join("symlink.json");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(write_to(&link, &entry).is_err());
            assert!(read_from(
                &link,
                "local:abc",
                "https://api.contextstream.io",
                Some("ws-1"),
                Some("proj-1"),
                GATE_POLICY_CACHE_TTL,
            )
            .is_none());
        }
    }

    #[tokio::test]
    async fn cached_gate_policy_uses_disk_cache_and_honors_off_switch() {
        let _guard = crate::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_switch = std::env::var_os(command_hints::ENV_SWITCH);
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        std::env::remove_var(command_hints::ENV_SWITCH);

        let payload = serde_json::json!({
            "success": true,
            "data": {
                "workspace_id": "ws-1",
                "project_id": null,
                "inherited": true,
                "policy": {
                    "require_verified_tree_before_push": true,
                    "required_command_families": ["cargo test"],
                    "receipt_ttl_secs": 3600
                }
            }
        });
        let (url, handle) = mock_once("200 OK", &payload.to_string()).await;
        let client = ContextStreamClient::new(Config {
            api_url: url.clone(),
            api_key: Some("k".into()),
            ..Default::default()
        });
        let identity = command_hints::local_identity("secret-key");

        let first = cached_gate_policy(
            &client,
            &identity,
            Some("ws-1"),
            None,
            Duration::from_secs(2),
        )
        .await
        .expect("fetched from API");
        assert!(first.policy.require_verified_tree_before_push);
        let _ = handle.await.unwrap();

        // Second call uses the cached file without hitting the mock server (which only accepted 1 conn).
        let second = cached_gate_policy(
            &client,
            &identity,
            Some("ws-1"),
            None,
            Duration::from_secs(2),
        )
        .await
        .expect("served from disk cache");
        assert_eq!(first, second);

        // Switching CONTEXTSTREAM_HOSTED_TOOLS=off bypasses the cache and API.
        std::env::set_var(command_hints::ENV_SWITCH, "off");
        assert!(cached_gate_policy(
            &client,
            &identity,
            Some("ws-1"),
            None,
            Duration::from_secs(2),
        )
        .await
        .is_none());

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_switch {
            Some(v) => std::env::set_var(command_hints::ENV_SWITCH, v),
            None => std::env::remove_var(command_hints::ENV_SWITCH),
        }
    }
}
