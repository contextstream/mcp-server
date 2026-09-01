//! Privacy-bounded remote synchronization for the local harness ledger.
//!
//! Upload is best-effort and never participates in MCP tool success. Only the
//! current managed runtime's harness is eligible, and the wire model has no
//! free-form metadata field through which prompts, paths, hostnames, usernames,
//! tool arguments, or model output could leak.

use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use dashmap::DashMap;
use mcp_types::{
    Error, HarnessId, HarnessReadinessEvidence, HarnessReadinessStage, ReadinessEvidenceSource,
    ReadinessEvidenceStatus, Result, HARNESS_READINESS_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    activation,
    client::{ContextStreamClient, RequestOptions},
    harness_readiness,
};

pub const REMOTE_HARNESS_READINESS_EVENT_SCHEMA_VERSION: i16 = 1;
const EVENT_NAMESPACE: Uuid = Uuid::from_bytes([
    0x6e, 0x1b, 0x22, 0x72, 0x86, 0x98, 0x52, 0x8e, 0xb8, 0x66, 0x91, 0x55, 0x03, 0x9f, 0x4e, 0x5f,
]);
const MAX_PROCESS_EVENT_CACHE: usize = 4096;
static DELIVERED_EVENTS: OnceLock<DashMap<Uuid, ()>> = OnceLock::new();
static IN_FLIGHT_EVENTS: OnceLock<DashMap<Uuid, ()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteHarnessReadinessEvent {
    pub event_id: Uuid,
    pub event_schema_version: i16,
    pub occurred_at: DateTime<Utc>,
    pub installation_id: Uuid,
    pub harness_id: HarnessId,
    pub stage: HarnessReadinessStage,
    pub status: ReadinessEvidenceStatus,
    pub source: ReadinessEvidenceSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teaching_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_config_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_hash: Option<String>,
}

impl RemoteHarnessReadinessEvent {
    pub fn from_local(
        installation_id: Uuid,
        evidence: &HarnessReadinessEvidence,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) -> Self {
        let mut event = Self {
            event_id: Uuid::nil(),
            event_schema_version: REMOTE_HARNESS_READINESS_EVENT_SCHEMA_VERSION,
            occurred_at: evidence.observed_at,
            installation_id,
            harness_id: evidence.harness_id,
            stage: evidence.stage,
            status: evidence.status,
            source: evidence.source,
            workspace_id,
            project_id,
            teaching_version: evidence.teaching_version.clone(),
            managed_config_version: evidence.managed_config_version.clone(),
            rules_hash: evidence.rules_hash.clone(),
        };
        event.event_id = deterministic_event_id(&event);
        event
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HarnessReadinessSyncSummary {
    pub eligible: usize,
    pub attempted: usize,
    pub delivered: usize,
    pub inserted: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct HarnessReadinessIngestResponse {
    inserted: bool,
    #[allow(dead_code)]
    current_updated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RemoteCurrentHarnessReadiness {
    pub installation_id: Uuid,
    pub harness_id: HarnessId,
    pub stage: HarnessReadinessStage,
    pub status: ReadinessEvidenceStatus,
    pub source: ReadinessEvidenceSource,
    pub occurred_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub last_event_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub teaching_version: Option<String>,
    pub managed_config_version: Option<String>,
    pub rules_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HarnessReadinessStatusResponse {
    pub event_schema_version: i16,
    pub installation_id: Uuid,
    pub harness_id: HarnessId,
    pub evidence: Vec<RemoteCurrentHarnessReadiness>,
}

fn remote_harness_readiness_enabled_value(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

pub fn remote_harness_readiness_enabled() -> bool {
    remote_harness_readiness_enabled_value(
        std::env::var("CONTEXTSTREAM_HARNESS_READINESS_REMOTE_ENABLED")
            .ok()
            .as_deref(),
    )
}

fn canonical_field(buffer: &mut Vec<u8>, value: &str) {
    buffer.extend_from_slice(&(value.len() as u64).to_be_bytes());
    buffer.extend_from_slice(value.as_bytes());
}

fn optional_uuid(value: Option<Uuid>) -> String {
    value.map(|id| id.to_string()).unwrap_or_default()
}

fn deterministic_event_id(event: &RemoteHarnessReadinessEvent) -> Uuid {
    let mut canonical = Vec::with_capacity(512);
    for field in [
        event.event_schema_version.to_string(),
        event
            .occurred_at
            .to_rfc3339_opts(SecondsFormat::Nanos, true),
        event.installation_id.to_string(),
        event.harness_id.as_str().to_string(),
        serde_json::to_string(&event.stage).unwrap_or_default(),
        serde_json::to_string(&event.status).unwrap_or_default(),
        serde_json::to_string(&event.source).unwrap_or_default(),
        optional_uuid(event.workspace_id),
        optional_uuid(event.project_id),
        event.teaching_version.clone().unwrap_or_default(),
        event.managed_config_version.clone().unwrap_or_default(),
        event
            .rules_hash
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default(),
    ] {
        canonical_field(&mut canonical, &field);
    }
    let mut digest = Sha256::new();
    digest.update(EVENT_NAMESPACE.as_bytes());
    digest.update(canonical);
    let digest = digest.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562 custom UUID (version 8) with the standard variant bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn runtime_harness() -> Option<(activation::RuntimeMetadata, HarnessId)> {
    let metadata = activation::runtime_metadata();
    let harness = metadata
        .client_name
        .as_deref()
        .and_then(HarnessId::from_alias)?;
    Some((metadata, harness))
}

fn request_headers(event: &RemoteHarnessReadinessEvent) -> Vec<(String, String)> {
    let mut headers = vec![
        ("X-ContextStream-MCP-Runtime".to_string(), "1".to_string()),
        (
            "X-ContextStream-Installation-Id".to_string(),
            event.installation_id.to_string(),
        ),
        (
            "X-ContextStream-Client".to_string(),
            event.harness_id.as_str().to_string(),
        ),
    ];
    if let Some(version) = &event.managed_config_version {
        headers.push((
            "X-ContextStream-Managed-Config-Version".to_string(),
            version.clone(),
        ));
    }
    if let Some(version) = &event.teaching_version {
        headers.push((
            "X-ContextStream-Teaching-Version".to_string(),
            version.clone(),
        ));
    }
    headers
}

fn status_headers(installation_id: Uuid, harness_id: HarnessId) -> Vec<(String, String)> {
    vec![
        ("X-ContextStream-MCP-Runtime".to_string(), "1".to_string()),
        (
            "X-ContextStream-Installation-Id".to_string(),
            installation_id.to_string(),
        ),
        (
            "X-ContextStream-Client".to_string(),
            harness_id.as_str().to_string(),
        ),
    ]
}

impl ContextStreamClient {
    async fn deliver_harness_readiness_event(
        &self,
        event: &RemoteHarnessReadinessEvent,
    ) -> Result<HarnessReadinessIngestResponse> {
        self.request(
            "POST",
            "/harness-readiness/events",
            Some(event.clone()),
            Some(RequestOptions {
                timeout: Some(Duration::from_secs(3)),
                retries: Some(1),
                extra_headers: Some(request_headers(event)),
                ..Default::default()
            }),
        )
        .await
    }

    async fn deliver_harness_readiness_events(
        &self,
        events: &[RemoteHarnessReadinessEvent],
    ) -> Result<HarnessReadinessSyncSummary> {
        let delivered_events = DELIVERED_EVENTS.get_or_init(DashMap::new);
        let in_flight_events = IN_FLIGHT_EVENTS.get_or_init(DashMap::new);
        // Backend event ids are idempotent, so bounded cache eviction can only
        // cause a harmless retry; it can never duplicate a stored event.
        if delivered_events.len() > MAX_PROCESS_EVENT_CACHE {
            delivered_events.clear();
        }
        if in_flight_events.len() > MAX_PROCESS_EVENT_CACHE {
            in_flight_events.clear();
        }
        let mut summary = HarnessReadinessSyncSummary {
            eligible: events.len(),
            ..Default::default()
        };
        let mut first_error = None;

        for event in events {
            if delivered_events.contains_key(&event.event_id) {
                continue;
            }
            if in_flight_events.insert(event.event_id, ()).is_some() {
                continue;
            }
            summary.attempted += 1;
            let delivery = self.deliver_harness_readiness_event(event).await;
            in_flight_events.remove(&event.event_id);
            match delivery {
                Ok(response) => {
                    delivered_events.insert(event.event_id, ());
                    summary.delivered += 1;
                    summary.inserted += usize::from(response.inserted);
                }
                Err(error) => {
                    summary.failed += 1;
                    tracing::debug!(
                        event_id = %event.event_id,
                        harness_id = event.harness_id.as_str(),
                        stage = ?event.stage,
                        source = ?event.source,
                        %error,
                        "individual harness readiness event delivery failed"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if summary.attempted > 0 && summary.failed == summary.attempted {
            return Err(first_error.unwrap_or_else(|| {
                Error::Network("all harness readiness event deliveries failed".to_string())
            }));
        }
        Ok(summary)
    }

    /// Upload the current managed harness's local evidence.
    ///
    /// This explicit method returns transport errors for doctor/tests. Runtime
    /// callers should use [`Self::spawn_harness_readiness_sync`] so telemetry
    /// can never delay or fail a tool response.
    pub async fn sync_local_harness_readiness(&self) -> Result<HarnessReadinessSyncSummary> {
        if !remote_harness_readiness_enabled() {
            return Ok(HarnessReadinessSyncSummary::default());
        }
        let config = self.config().await;
        if config.is_http_transport {
            return Ok(HarnessReadinessSyncSummary::default());
        }
        let Some((metadata, harness_id)) = runtime_harness() else {
            return Ok(HarnessReadinessSyncSummary::default());
        };
        let Some(ledger) = harness_readiness::read_harness_readiness().map_err(|error| {
            Error::Validation(format!("Could not read harness readiness ledger: {error}"))
        })?
        else {
            return Ok(HarnessReadinessSyncSummary::default());
        };
        if ledger.installation_id != metadata.installation_id {
            return Err(Error::Validation(
                "Harness readiness ledger installation identity does not match runtime".to_string(),
            ));
        }

        let events: Vec<RemoteHarnessReadinessEvent> = ledger
            .evidence
            .iter()
            .filter(|evidence| evidence.harness_id == harness_id)
            .filter(|evidence| evidence.schema_version == HARNESS_READINESS_SCHEMA_VERSION)
            .map(|evidence| {
                RemoteHarnessReadinessEvent::from_local(
                    ledger.installation_id,
                    evidence,
                    None,
                    None,
                )
            })
            .collect();
        self.deliver_harness_readiness_events(&events).await
    }

    /// Detached, best-effort readiness upload. Errors are diagnostic only.
    pub fn spawn_harness_readiness_sync(&self) {
        if !remote_harness_readiness_enabled() {
            return;
        }
        let client = self.clone();
        crate::spawn_with_task_context(async move {
            if let Err(error) = client.sync_local_harness_readiness().await {
                tracing::debug!(%error, "harness readiness delivery skipped");
            }
        });
    }

    /// Upload one privacy-bounded runtime observation from a managed remote
    /// MCP connection. Unlike local sync, this path never reads the gateway's
    /// filesystem and is safe for shared HTTP processes. Delivery is detached
    /// with the caller's task-local auth/scope/session restored.
    pub fn spawn_runtime_harness_readiness(
        &self,
        installation_id: Uuid,
        evidence: HarnessReadinessEvidence,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) {
        self.spawn_runtime_harness_readiness_if_enabled(
            remote_harness_readiness_enabled(),
            installation_id,
            evidence,
            workspace_id,
            project_id,
        );
    }

    fn spawn_runtime_harness_readiness_if_enabled(
        &self,
        enabled: bool,
        installation_id: Uuid,
        evidence: HarnessReadinessEvidence,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) {
        if !enabled
            || installation_id.is_nil()
            || evidence.schema_version != HARNESS_READINESS_SCHEMA_VERSION
        {
            return;
        }
        let event = RemoteHarnessReadinessEvent::from_local(
            installation_id,
            &evidence,
            workspace_id,
            project_id,
        );
        let client = self.clone();
        crate::spawn_with_task_context(async move {
            if let Err(error) = client
                .deliver_harness_readiness_events(std::slice::from_ref(&event))
                .await
            {
                tracing::debug!(
                    event_id = %event.event_id,
                    harness_id = event.harness_id.as_str(),
                    stage = ?event.stage,
                    source = ?event.source,
                    %error,
                    "runtime harness readiness delivery skipped"
                );
            }
        });
    }

    /// Fetch server-projected evidence for this managed installation/harness.
    pub async fn harness_readiness_status(
        &self,
        harness_id: HarnessId,
    ) -> Result<HarnessReadinessStatusResponse> {
        let metadata = activation::runtime_metadata();
        if metadata
            .client_name
            .as_deref()
            .and_then(HarnessId::from_alias)
            != Some(harness_id)
        {
            return Err(Error::Validation(
                "Requested harness does not match the managed runtime".to_string(),
            ));
        }
        self.harness_readiness_status_for_installation(metadata.installation_id, harness_id)
            .await
    }

    /// Fetch server-projected evidence for an existing managed installation.
    ///
    /// Doctor uses this explicit form because it diagnoses configured harnesses
    /// outside an active editor process. The request remains authenticated and
    /// the server scopes every row to that authenticated user; this method does
    /// not create installation state or accept a nil identity.
    pub async fn harness_readiness_status_for_installation(
        &self,
        installation_id: Uuid,
        harness_id: HarnessId,
    ) -> Result<HarnessReadinessStatusResponse> {
        let config = self.config().await;
        if config.is_http_transport {
            return Err(Error::Validation(
                "Harness readiness status is available only to a managed local runtime".to_string(),
            ));
        }
        if installation_id.is_nil() {
            return Err(Error::Validation(
                "Harness readiness status requires an existing installation identity".to_string(),
            ));
        }
        let path = format!(
            "/harness-readiness/status?installation_id={}&harness_id={}",
            installation_id,
            harness_id.as_str()
        );
        let response: HarnessReadinessStatusResponse = self
            .request(
                "GET",
                &path,
                None::<()>,
                Some(RequestOptions {
                    timeout: Some(Duration::from_secs(3)),
                    retries: Some(1),
                    extra_headers: Some(status_headers(installation_id, harness_id)),
                    ..Default::default()
                }),
            )
            .await?;
        if response.event_schema_version != REMOTE_HARNESS_READINESS_EVENT_SCHEMA_VERSION
            || response.installation_id != installation_id
            || response.harness_id != harness_id
            || response.evidence.iter().any(|evidence| {
                evidence.installation_id != installation_id || evidence.harness_id != harness_id
            })
        {
            return Err(Error::Validation(
                "Harness readiness status response identity or schema did not match the request"
                    .to_string(),
            ));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::{AuthOverride, SessionKey};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn remote_readiness_is_explicitly_opt_in() {
        assert!(!remote_harness_readiness_enabled_value(None));
        assert!(!remote_harness_readiness_enabled_value(Some("")));
        assert!(!remote_harness_readiness_enabled_value(Some("false")));
        assert!(!remote_harness_readiness_enabled_value(Some("unexpected")));
        for enabled in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(
                remote_harness_readiness_enabled_value(Some(enabled)),
                "{enabled}"
            );
        }
    }

    fn evidence() -> HarnessReadinessEvidence {
        let mut evidence = HarnessReadinessEvidence::new(
            HarnessId::Codex,
            HarnessReadinessStage::Taught,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ManagedRules,
            DateTime::parse_from_rfc3339("2026-07-28T20:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        evidence.teaching_version = Some("harness_teaching_v4".to_string());
        evidence.rules_hash = Some("0123456789abcdef".to_string());
        evidence
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        const MAX_REQUEST_BYTES: usize = 64 * 1024;
        let mut request = Vec::with_capacity(4096);
        let mut chunk = [0_u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.expect("read HTTP request");
            assert!(read > 0, "HTTP request ended before its body was complete");
            request.extend_from_slice(&chunk[..read]);
            assert!(
                request.len() <= MAX_REQUEST_BYTES,
                "test HTTP request exceeded its safety bound"
            );

            let Some(header_end) = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= header_end + content_length {
                return String::from_utf8(request).expect("HTTP request is UTF-8");
            }
        }
    }

    async fn readiness_client_with_responses(
        responses: Vec<(&'static str, String)>,
    ) -> (ContextStreamClient, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind readiness test listener");
        let address = listener.local_addr().expect("readiness listener address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("accept readiness request");
                requests.push(read_http_request(&mut socket).await);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write readiness response");
            }
            requests
        });

        let config = mcp_types::Config {
            api_url: format!("http://{address}"),
            api_key: Some("readiness-test-key".to_string()),
            is_http_transport: false,
            ..Default::default()
        };
        (ContextStreamClient::new(config), server)
    }

    fn header_values<'a>(request: &'a str, expected_name: &str) -> Vec<&'a str> {
        request
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(expected_name)
                    .then_some(value.trim())
            })
            .collect()
    }

    #[test]
    fn deterministic_event_id_is_stable_and_payload_sensitive() {
        let installation_id = Uuid::parse_str("10101010-1010-4010-8010-101010101010").unwrap();
        let first =
            RemoteHarnessReadinessEvent::from_local(installation_id, &evidence(), None, None);
        let second =
            RemoteHarnessReadinessEvent::from_local(installation_id, &evidence(), None, None);
        assert_eq!(first.event_id, second.event_id);

        let mut changed = evidence();
        changed.rules_hash = Some("fedcba9876543210".to_string());
        let changed =
            RemoteHarnessReadinessEvent::from_local(installation_id, &changed, None, None);
        assert_ne!(first.event_id, changed.event_id);
    }

    #[test]
    fn wire_payload_is_strict_and_contains_no_free_form_metadata() {
        let payload =
            RemoteHarnessReadinessEvent::from_local(Uuid::new_v4(), &evidence(), None, None);
        let value = serde_json::to_value(payload).unwrap();
        let object = value.as_object().unwrap();
        assert!(object.len() <= 14);
        for forbidden in [
            "prompt",
            "content",
            "path",
            "hostname",
            "username",
            "properties",
            "metadata",
            "error",
        ] {
            assert!(!object.contains_key(forbidden));
        }
    }

    #[test]
    fn critical_headers_are_exact_and_version_scoped() {
        let payload =
            RemoteHarnessReadinessEvent::from_local(Uuid::new_v4(), &evidence(), None, None);
        let headers = request_headers(&payload);
        assert!(headers
            .iter()
            .any(|(name, value)| name == "X-ContextStream-Client" && value == "codex"));
        assert!(headers.iter().any(|(name, value)| {
            name == "X-ContextStream-Teaching-Version" && value == "harness_teaching_v4"
        }));
        assert!(!headers
            .iter()
            .any(|(name, _)| name == "X-ContextStream-Managed-Config-Version"));
    }

    #[tokio::test]
    async fn partial_delivery_retries_only_failed_events_without_duplicate_headers() {
        let installation_id = Uuid::new_v4();
        let first =
            RemoteHarnessReadinessEvent::from_local(installation_id, &evidence(), None, None);
        let mut changed = evidence();
        changed.rules_hash = Some("fedcba9876543210".to_string());
        let second = RemoteHarnessReadinessEvent::from_local(installation_id, &changed, None, None);
        let (client, server) = readiness_client_with_responses(vec![
            (
                "409 Conflict",
                serde_json::json!({
                    "error": {
                        "code": "event_id_collision",
                        "message": "event id collision"
                    }
                })
                .to_string(),
            ),
            (
                "200 OK",
                serde_json::json!({
                    "success": true,
                    "data": {
                        "inserted": true,
                        "current_updated": true
                    }
                })
                .to_string(),
            ),
            (
                "200 OK",
                serde_json::json!({
                    "success": true,
                    "data": {
                        "inserted": true,
                        "current_updated": true
                    }
                })
                .to_string(),
            ),
        ])
        .await;

        let summary = client
            .deliver_harness_readiness_events(&[first.clone(), second.clone()])
            .await
            .expect("one failed event must not block a later successful event");
        assert_eq!(
            summary,
            HarnessReadinessSyncSummary {
                eligible: 2,
                attempted: 2,
                delivered: 1,
                inserted: 1,
                failed: 1,
            }
        );
        let retry = client
            .deliver_harness_readiness_events(&[first.clone(), second.clone()])
            .await
            .expect("a later sync must retry only the event that failed");
        assert_eq!(
            retry,
            HarnessReadinessSyncSummary {
                eligible: 2,
                attempted: 1,
                delivered: 1,
                inserted: 1,
                failed: 0,
            }
        );

        let requests = server.await.expect("readiness server task");
        assert_eq!(requests.len(), 3);
        let event_ids: Vec<Uuid> = requests
            .iter()
            .map(|request| {
                let (_, body) = request
                    .split_once("\r\n\r\n")
                    .expect("HTTP request has a header/body boundary");
                serde_json::from_str::<RemoteHarnessReadinessEvent>(body)
                    .expect("decode readiness request")
                    .event_id
            })
            .collect();
        assert_eq!(
            event_ids,
            [first.event_id, second.event_id, first.event_id],
            "the successful event must be skipped while the failed event is retried"
        );
        for request in &requests {
            assert_eq!(header_values(request, "x-contextstream-mcp-runtime"), ["1"]);
            assert_eq!(
                header_values(request, "x-contextstream-installation-id"),
                [installation_id.to_string()]
            );
            assert_eq!(header_values(request, "x-contextstream-client"), ["codex"]);
            assert_eq!(
                header_values(request, "x-contextstream-teaching-version"),
                ["harness_teaching_v4"]
            );
            assert!(header_values(request, "x-contextstream-managed-config-version").is_empty());
        }
    }

    #[tokio::test]
    async fn all_failed_delivery_still_attempts_every_event_and_returns_error() {
        let installation_id = Uuid::new_v4();
        let first =
            RemoteHarnessReadinessEvent::from_local(installation_id, &evidence(), None, None);
        let mut changed = evidence();
        changed.rules_hash = Some("fedcba9876543210".to_string());
        let second = RemoteHarnessReadinessEvent::from_local(installation_id, &changed, None, None);
        let error_body = serde_json::json!({
            "error": {
                "code": "readiness_rejected",
                "message": "readiness rejected"
            }
        })
        .to_string();
        let (client, server) = readiness_client_with_responses(vec![
            ("409 Conflict", error_body.clone()),
            ("409 Conflict", error_body),
        ])
        .await;

        assert!(client
            .deliver_harness_readiness_events(&[first, second])
            .await
            .is_err());
        assert_eq!(
            server.await.expect("readiness server task").len(),
            2,
            "an early event failure must not prevent later attempts"
        );
    }

    #[tokio::test]
    async fn detached_runtime_delivery_preserves_caller_auth_and_identity() {
        let installation_id = Uuid::new_v4();
        let mut runtime_evidence = HarnessReadinessEvidence::new(
            HarnessId::Codex,
            HarnessReadinessStage::Practicing,
            ReadinessEvidenceStatus::Inferred,
            ReadinessEvidenceSource::RuntimeBehavior,
            Utc::now(),
        );
        runtime_evidence.teaching_version = Some("harness_teaching_v4".to_string());
        let (client, server) = readiness_client_with_responses(vec![(
            "200 OK",
            serde_json::json!({
                "success": true,
                "data": {
                    "inserted": true,
                    "current_updated": true
                }
            })
            .to_string(),
        )])
        .await;

        crate::run_with_session_key(
            SessionKey::ApiKey("caller-partition".to_string()),
            || async {
                crate::run_with_auth_override(
                    AuthOverride {
                        api_key: Some("caller-request-key".to_string()),
                        ..Default::default()
                    },
                    || async {
                        client.spawn_runtime_harness_readiness_if_enabled(
                            true,
                            installation_id,
                            runtime_evidence,
                            None,
                            None,
                        );
                    },
                )
                .await
            },
        )
        .await;

        let requests = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("runtime readiness request")
            .expect("readiness server task");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            header_values(&requests[0], "x-api-key"),
            ["caller-request-key"]
        );
        assert_eq!(
            header_values(&requests[0], "x-contextstream-installation-id"),
            [installation_id.to_string()]
        );
        assert_eq!(
            header_values(&requests[0], "x-contextstream-client"),
            ["codex"]
        );
        let (_, body) = requests[0]
            .split_once("\r\n\r\n")
            .expect("HTTP request body");
        let event: RemoteHarnessReadinessEvent =
            serde_json::from_str(body).expect("runtime readiness event");
        assert_eq!(event.stage, HarnessReadinessStage::Practicing);
        assert_eq!(event.status, ReadinessEvidenceStatus::Inferred);
        assert_eq!(event.source, ReadinessEvidenceSource::RuntimeBehavior);
    }

    #[tokio::test]
    async fn explicit_status_query_is_scoped_and_runtime_headers_are_unique() {
        let installation_id = Uuid::new_v4();
        let (client, server) = readiness_client_with_responses(vec![(
            "200 OK",
            serde_json::json!({
                "success": true,
                "data": {
                    "event_schema_version": REMOTE_HARNESS_READINESS_EVENT_SCHEMA_VERSION,
                    "installation_id": installation_id,
                    "harness_id": "codex",
                    "evidence": []
                }
            })
            .to_string(),
        )])
        .await;

        let status = client
            .harness_readiness_status_for_installation(installation_id, HarnessId::Codex)
            .await
            .expect("fetch explicit managed-installation status");
        assert_eq!(status.installation_id, installation_id);
        assert_eq!(status.harness_id, HarnessId::Codex);

        let requests = server.await.expect("readiness status server task");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(request.starts_with(&format!(
            "GET /api/v1/harness-readiness/status?installation_id={installation_id}&harness_id=codex "
        )));
        assert_eq!(header_values(request, "x-contextstream-mcp-runtime"), ["1"]);
        assert_eq!(
            header_values(request, "x-contextstream-installation-id"),
            [installation_id.to_string()]
        );
        assert_eq!(header_values(request, "x-contextstream-client"), ["codex"]);
    }

    #[tokio::test]
    async fn explicit_status_query_rejects_nil_installation_without_network_io() {
        let client = ContextStreamClient::new(mcp_types::Config::default());
        assert!(client
            .harness_readiness_status_for_installation(Uuid::nil(), HarnessId::Codex)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn explicit_status_query_rejects_mismatched_response_identity() {
        let installation_id = Uuid::new_v4();
        let (client, server) = readiness_client_with_responses(vec![(
            "200 OK",
            serde_json::json!({
                "success": true,
                "data": {
                    "event_schema_version": REMOTE_HARNESS_READINESS_EVENT_SCHEMA_VERSION,
                    "installation_id": Uuid::new_v4(),
                    "harness_id": "codex",
                    "evidence": []
                }
            })
            .to_string(),
        )])
        .await;

        assert!(client
            .harness_readiness_status_for_installation(installation_id, HarnessId::Codex)
            .await
            .is_err());
        assert_eq!(server.await.expect("readiness status server task").len(), 1);
    }
}
