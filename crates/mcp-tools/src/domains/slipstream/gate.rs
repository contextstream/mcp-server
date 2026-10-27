//! Whether a caller's workspace has Slipstream, cached per caller and workspace.
//!
//! The `agents` tool is listed only when the catalog answers `200` with at least one
//! agent an owner or admin turned on; otherwise it costs the session no prompt. The answer
//! depends only on who asks and for which workspace, so it is cached under that pair:
//!
//! - available or none on: 5 minutes; refused (`401`/`403`, or an API without the route):
//!   10 minutes;
//! - unreachable (network, timeout, `5xx`): retried after 30 s, and a catalog known before the
//!   failure keeps being served meanwhile, so a blip never takes the tool away mid-session.
//!
//! `find` and `run` always read the catalog fresh and record it here, which is how an agent
//! turned on or off reaches the listing between refreshes.

use mcp_client::ContextStreamClient;
use mcp_types::Error;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::catalog::Catalog;

pub(crate) const AVAILABLE_TTL: Duration = Duration::from_secs(300);
pub(crate) const REFUSED_TTL: Duration = Duration::from_secs(600);
pub(crate) const UNREACHABLE_RETRY: Duration = Duration::from_secs(30);
/// Callers and workspaces remembered at once; the oldest answers go first beyond it.
pub(crate) const MAX_ENTRIES: usize = 4096;

/// What the catalog said for one caller and workspace.
#[derive(Debug, Clone)]
pub(crate) enum Availability {
    /// `200` with at least one agent turned on.
    Available(Arc<Catalog>),
    /// `200`, but no agent is turned on.
    NoneEnabled(Arc<Catalog>),
    /// The workspace is not open to Slipstream for this caller (`401`/`403`, or `404` from an
    /// API without the catalog route).
    Refused(String),
    /// The catalog could not be read (network, timeout, `5xx`, a body that is not a catalog).
    Unreachable(String),
}

impl Availability {
    pub fn is_listed(&self) -> bool {
        matches!(self, Self::Available(_))
    }

    pub fn catalog(&self) -> Option<&Arc<Catalog>> {
        match self {
            Self::Available(catalog) | Self::NoneEnabled(catalog) => Some(catalog),
            _ => None,
        }
    }

    fn ttl(&self) -> Duration {
        match self {
            Self::Available(_) | Self::NoneEnabled(_) => AVAILABLE_TTL,
            Self::Refused(_) => REFUSED_TTL,
            Self::Unreachable(_) => UNREACHABLE_RETRY,
        }
    }

    /// Classify one catalog read.
    pub fn from_read(read: std::result::Result<Value, Error>) -> Self {
        match read {
            Ok(value) => {
                let has_agents = value
                    .get("data")
                    .unwrap_or(&value)
                    .get("agents")
                    .is_some_and(Value::is_array);
                if !has_agents {
                    return Self::Unreachable("the catalog answer had no agents list".to_string());
                }
                let catalog = Arc::new(Catalog::from_value(&value));
                if catalog.has_enabled() {
                    Self::Available(catalog)
                } else {
                    Self::NoneEnabled(catalog)
                }
            }
            Err(Error::Http {
                status: 401 | 403 | 404,
                message,
                ..
            }) => Self::Refused(message),
            Err(error) => Self::Unreachable(error.to_string()),
        }
    }
}

/// One cached answer.
#[derive(Debug, Clone)]
pub(crate) struct GateEntry {
    pub availability: Availability,
    /// When the answer was read from the API.
    pub read_at: Instant,
    /// When it should be read again.
    pub refresh_at: Instant,
}

impl GateEntry {
    pub fn is_fresh_at(&self, now: Instant) -> bool {
        now < self.refresh_at
    }

    pub fn is_fresh(&self) -> bool {
        self.is_fresh_at(Instant::now())
    }
}

/// Who asks, about which workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GateKey {
    /// A secret-free identity of the caller (see [`super::caller_identity`]).
    pub identity: String,
    pub workspace_id: Uuid,
}

/// The process-wide Slipstream availability cache.
#[derive(Default)]
pub struct SlipstreamGate {
    entries: Mutex<HashMap<GateKey, GateEntry>>,
    /// One refresh at a time per key: a listing waiting on a prefetch reuses its answer.
    refreshing: Mutex<HashMap<GateKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Per caller identity: the fingerprint of the listing last served, for
    /// `notifications/tools/list_changed`.
    advertised: Mutex<HashMap<String, u64>>,
}

impl SlipstreamGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The gate every registry shares: the hosted gateway builds a registry per request.
    pub fn global() -> Arc<Self> {
        static GATE: OnceLock<Arc<SlipstreamGate>> = OnceLock::new();
        GATE.get_or_init(|| Arc::new(Self::new())).clone()
    }

    /// How many answers are cached.
    #[cfg(test)]
    pub(crate) fn cached_count(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub(crate) fn entry(&self, key: &GateKey) -> Option<GateEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    pub(crate) fn record(&self, key: &GateKey, availability: Availability) -> GateEntry {
        self.record_at(key, availability, Instant::now())
    }

    /// Store one answer. An unreachable catalog never replaces one read before: the earlier
    /// answer stays, to be read again after [`UNREACHABLE_RETRY`].
    pub(crate) fn record_at(
        &self,
        key: &GateKey,
        availability: Availability,
        now: Instant,
    ) -> GateEntry {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = match (&availability, entries.get(key)) {
            (Availability::Unreachable(_), Some(previous))
                if !matches!(previous.availability, Availability::Unreachable(_)) =>
            {
                GateEntry {
                    availability: previous.availability.clone(),
                    read_at: previous.read_at,
                    refresh_at: now + UNREACHABLE_RETRY,
                }
            }
            _ => GateEntry {
                refresh_at: now + availability.ttl(),
                availability,
                read_at: now,
            },
        };
        entries.insert(key.clone(), entry.clone());
        if entries.len() > MAX_ENTRIES {
            Self::evict(&mut entries, now);
        }
        entry
    }

    fn evict(entries: &mut HashMap<GateKey, GateEntry>, now: Instant) {
        entries.retain(|_, entry| entry.refresh_at + REFUSED_TTL > now);
        while entries.len() > MAX_ENTRIES {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.read_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
    }

    /// The cached answer when it is fresh; otherwise read the catalog, waiting at most
    /// `budget`. A read that does not finish in time counts as unreachable.
    pub(crate) async fn ensure(
        &self,
        client: &ContextStreamClient,
        key: &GateKey,
        budget: Duration,
    ) -> GateEntry {
        if let Some(entry) = self.entry(key).filter(GateEntry::is_fresh) {
            return entry;
        }
        let started = Instant::now();
        let lock = {
            let mut refreshing = self.refreshing.lock().unwrap_or_else(|e| e.into_inner());
            if refreshing.len() > MAX_ENTRIES {
                refreshing.retain(|_, lock| Arc::strong_count(lock) > 1);
            }
            refreshing.entry(key.clone()).or_default().clone()
        };
        let Ok(_guard) = tokio::time::timeout(budget, lock.lock()).await else {
            return self.unreachable_or_stale(key, "the catalog read is still running");
        };
        // Someone else may have read it while this waited.
        if let Some(entry) = self.entry(key).filter(GateEntry::is_fresh) {
            return entry;
        }
        let remaining = budget.saturating_sub(started.elapsed());
        let availability = read_catalog(client, key.workspace_id, remaining).await;
        self.record(key, availability)
    }

    fn unreachable_or_stale(&self, key: &GateKey, reason: &str) -> GateEntry {
        self.entry(key).unwrap_or_else(|| {
            let now = Instant::now();
            GateEntry {
                availability: Availability::Unreachable(reason.to_string()),
                read_at: now,
                refresh_at: now,
            }
        })
    }

    /// The fingerprint of the listing last served to `identity`.
    pub(crate) fn advertised(&self, identity: &str) -> Option<u64> {
        self.advertised
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(identity)
            .copied()
    }

    pub(crate) fn set_advertised(&self, identity: &str, fingerprint: u64) {
        let mut advertised = self.advertised.lock().unwrap_or_else(|e| e.into_inner());
        if advertised.len() >= MAX_ENTRIES && !advertised.contains_key(identity) {
            advertised.clear();
        }
        advertised.insert(identity.to_string(), fingerprint);
    }
}

/// Read the catalog once, with no retry and no session-refresh replay, inside `budget`.
pub(crate) async fn read_catalog(
    client: &ContextStreamClient,
    workspace_id: Uuid,
    budget: Duration,
) -> Availability {
    let path = format!("/tasks/agents?workspace_id={workspace_id}");
    match tokio::time::timeout(budget, client.get_once::<Value>(&path)).await {
        Ok(read) => Availability::from_read(read),
        Err(_) => Availability::Unreachable(format!(
            "the catalog did not answer within {} ms",
            budget.as_millis()
        )),
    }
}
