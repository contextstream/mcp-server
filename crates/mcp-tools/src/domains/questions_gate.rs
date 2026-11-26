//! Whether saved questions are switched on for a caller's workspace, cached per caller and
//! workspace.
//!
//! The API serves its `/questions` routes only where its saved-questions switch is on, and
//! there only to the workspaces of the pilot; for every other workspace they answer 404. So
//! `questions` is listed only for a caller whose current workspace has them switched on, and
//! everyone else pays no prompt for a tool that could only answer "not switched on". The
//! probe is `GET /questions/templates?workspace_id=<ws>`, which spends no credits and asks no
//! model. Its answer depends on who asks and for which workspace, so it is cached under that
//! pair:
//!
//! - switched on (`2xx`) or not (`401`, `403` or `404`; a key limited to one workspace is
//!   refused by these routes, which rightly leaves the tool out): 5 minutes;
//! - unreachable (network, timeout, `5xx`, any other answer): read again after 30 s, and an
//!   answer known before keeps being served meanwhile, so a blip never takes the tool away
//!   mid-session.
//!
//! Every `questions` call records what its answer says through [`QuestionsGate::record`] as
//! well, so a workspace switched on is noticed on its next call.

use mcp_client::ContextStreamClient;
use mcp_types::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a known answer (switched on or not) is served.
pub(crate) const KNOWN_TTL: Duration = Duration::from_secs(300);
/// How soon an unreachable probe is read again.
pub(crate) const UNREACHABLE_RETRY: Duration = Duration::from_secs(30);
/// The pause before a listing reads an unreachable probe again.
const RETRY_PAUSE: Duration = Duration::from_millis(400);
/// Callers and workspaces remembered at once; beyond it the oldest answers go first.
pub(crate) const MAX_ENTRIES: usize = 4096;

/// What the API said of saved questions for one caller and workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offer {
    /// Switched on: the routes answered.
    Offered,
    /// Not switched on for the workspace, or not for this caller (`401`, `403`, `404`).
    NotOffered,
    /// The probe could not be read (network, timeout, `5xx`, any other answer).
    Unreachable,
}

impl Offer {
    /// Classify one answer by its HTTP status.
    pub(crate) fn from_status(status: u16) -> Self {
        match status {
            200..=299 => Self::Offered,
            401 | 403 | 404 => Self::NotOffered,
            _ => Self::Unreachable,
        }
    }

    /// Classify one probe read: an answer the client took as a success was a `2xx`.
    pub(crate) fn from_read(read: &Result<Value>) -> Self {
        match read {
            Ok(_) => Self::Offered,
            Err(Error::Http { status, .. }) => Self::from_status(*status),
            Err(_) => Self::Unreachable,
        }
    }
}

/// Who asks, about which workspace: saved questions are switched on per workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GateKey {
    /// A secret-free identity of the caller (`slipstream::caller_identity`).
    pub identity: String,
    pub workspace_id: Uuid,
}

#[derive(Debug, Clone)]
struct Entry {
    offer: Offer,
    /// When the answer was read from the API.
    read_at: Instant,
    /// When it should be read again.
    refresh_at: Instant,
}

/// The process-wide cache of the workspaces saved questions are switched on for.
#[derive(Default)]
pub struct QuestionsGate {
    entries: Mutex<HashMap<GateKey, Entry>>,
}

impl QuestionsGate {
    /// An empty gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// The gate every registry shares (the hosted gateway builds a registry per request).
    pub fn global() -> Arc<Self> {
        static GATE: OnceLock<Arc<QuestionsGate>> = OnceLock::new();
        GATE.get_or_init(|| Arc::new(Self::new())).clone()
    }

    /// How many answers are cached.
    #[cfg(test)]
    pub(crate) fn cached_count(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The cached answer for `key`, when it is still fresh.
    pub(crate) fn fresh(&self, key: &GateKey) -> Option<Offer> {
        self.fresh_at(key, Instant::now())
    }

    /// The cached answer for `key`, when it is still fresh at `now`.
    pub(crate) fn fresh_at(&self, key: &GateKey, now: Instant) -> Option<Offer> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .filter(|entry| now < entry.refresh_at)
            .map(|entry| entry.offer)
    }

    /// Store one answer; returns the answer now served for `key`.
    pub(crate) fn record(&self, key: &GateKey, offer: Offer) -> Offer {
        self.record_at(key, offer, Instant::now())
    }

    /// Store one answer read at `now`. An unreachable probe never replaces an answer read
    /// before: that answer stays, to be read again after [`UNREACHABLE_RETRY`].
    pub(crate) fn record_at(&self, key: &GateKey, offer: Offer, now: Instant) -> Offer {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = match (offer, entries.get(key)) {
            (Offer::Unreachable, Some(previous)) if previous.offer != Offer::Unreachable => Entry {
                offer: previous.offer,
                read_at: previous.read_at,
                refresh_at: now + UNREACHABLE_RETRY,
            },
            (Offer::Unreachable, _) => Entry {
                offer,
                read_at: now,
                refresh_at: now + UNREACHABLE_RETRY,
            },
            _ => Entry {
                offer,
                read_at: now,
                refresh_at: now + KNOWN_TTL,
            },
        };
        let served = entry.offer;
        entries.insert(key.clone(), entry);
        if entries.len() > MAX_ENTRIES {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.read_at)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest);
            }
        }
        served
    }

    /// Whether `questions` is listed for `key` now: the cached answer when fresh, else the
    /// probe read within `budget` (a read that does not finish counts as unreachable).
    pub(crate) async fn offered(
        &self,
        client: &ContextStreamClient,
        key: &GateKey,
        budget: Duration,
    ) -> bool {
        if let Some(offer) = self.fresh(key) {
            return offer == Offer::Offered;
        }
        // A blip (a rolling deploy's 502, a cold connection) is read again once while the
        // budget lasts: a client that lists tools once a session would otherwise go
        // without the tool for the whole session.
        let started = Instant::now();
        let mut offer = Offer::Unreachable;
        for attempt in 0..2 {
            if attempt > 0 {
                let left = budget.saturating_sub(started.elapsed());
                if left.is_zero() {
                    break;
                }
                tokio::time::sleep(RETRY_PAUSE.min(left)).await;
            }
            let left = budget.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            offer = probe(client, key.workspace_id, left).await;
            if offer != Offer::Unreachable {
                break;
            }
        }
        self.record(key, offer) == Offer::Offered
    }
}

/// Ask whether saved questions are switched on for `workspace_id`: one read of the starter
/// templates, with no retry and no session-refresh replay, inside `budget`.
async fn probe(client: &ContextStreamClient, workspace_id: Uuid, budget: Duration) -> Offer {
    let path = format!("/questions/templates?workspace_id={workspace_id}");
    match tokio::time::timeout(budget, client.get_once::<Value>(&path)).await {
        Ok(read) => Offer::from_read(&read),
        Err(_) => Offer::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(identity: &str, workspace_id: Uuid) -> GateKey {
        GateKey {
            identity: identity.to_string(),
            workspace_id,
        }
    }

    #[test]
    fn the_status_decides_the_offer() {
        for status in [200, 201, 204, 299] {
            assert_eq!(Offer::from_status(status), Offer::Offered, "{status}");
        }
        // A workspace outside the pilot (404), and a caller these routes refuse, such as an
        // agent's key limited to one workspace (401, 403).
        for status in [401, 403, 404] {
            assert_eq!(Offer::from_status(status), Offer::NotOffered, "{status}");
        }
        for status in [500, 502, 503, 504, 400, 409, 429] {
            assert_eq!(Offer::from_status(status), Offer::Unreachable, "{status}");
        }
    }

    #[test]
    fn a_read_is_classified_by_its_answer() {
        assert_eq!(
            Offer::from_read(&Ok(json!({"templates": []}))),
            Offer::Offered
        );
        assert_eq!(
            Offer::from_read(&Err(Error::http(404, "Not Found"))),
            Offer::NotOffered
        );
        assert_eq!(
            Offer::from_read(&Err(Error::http(403, "Forbidden"))),
            Offer::NotOffered
        );
        assert_eq!(
            Offer::from_read(&Err(Error::http(401, "Unauthorized"))),
            Offer::NotOffered
        );
        for error in [
            Error::http(502, "Bad Gateway"),
            Error::Network("connection refused".to_string()),
            Error::Timeout(10),
        ] {
            assert_eq!(Offer::from_read(&Err(error)), Offer::Unreachable);
        }
    }

    #[test]
    fn known_answers_last_five_minutes_and_unreachable_ones_thirty_seconds() {
        let gate = QuestionsGate::new();
        let me = key("me", Uuid::new_v4());
        let t0 = Instant::now();
        assert_eq!(gate.record_at(&me, Offer::Offered, t0), Offer::Offered);
        assert_eq!(
            gate.fresh_at(&me, t0 + KNOWN_TTL - Duration::from_secs(1)),
            Some(Offer::Offered)
        );
        assert_eq!(gate.fresh_at(&me, t0 + KNOWN_TTL), None);

        let off = key("me", Uuid::new_v4());
        gate.record_at(&off, Offer::NotOffered, t0);
        assert_eq!(
            gate.fresh_at(&off, t0 + KNOWN_TTL - Duration::from_secs(1)),
            Some(Offer::NotOffered)
        );
        assert_eq!(gate.fresh_at(&off, t0 + KNOWN_TTL), None);

        // Nothing known before: unreachable, read again after 30 s.
        let down = key("me", Uuid::new_v4());
        assert_eq!(
            gate.record_at(&down, Offer::Unreachable, t0),
            Offer::Unreachable
        );
        assert_eq!(
            gate.fresh_at(&down, t0 + UNREACHABLE_RETRY - Duration::from_secs(1)),
            Some(Offer::Unreachable)
        );
        assert_eq!(gate.fresh_at(&down, t0 + UNREACHABLE_RETRY), None);
    }

    #[test]
    fn a_blip_keeps_the_answer_known_before() {
        let gate = QuestionsGate::new();
        let me = key("me", Uuid::new_v4());
        let t0 = Instant::now();
        gate.record_at(&me, Offer::Offered, t0);
        // Past its five minutes the probe fails: the known answer is still served, and read
        // again after 30 s.
        let t1 = t0 + KNOWN_TTL + Duration::from_secs(5);
        assert_eq!(gate.record_at(&me, Offer::Unreachable, t1), Offer::Offered);
        assert_eq!(
            gate.fresh_at(&me, t1 + UNREACHABLE_RETRY - Duration::from_secs(1)),
            Some(Offer::Offered)
        );
        assert_eq!(gate.fresh_at(&me, t1 + UNREACHABLE_RETRY), None);
        // So is a known "not switched on".
        let off = key("me", Uuid::new_v4());
        gate.record_at(&off, Offer::NotOffered, t0);
        assert_eq!(
            gate.record_at(&off, Offer::Unreachable, t1),
            Offer::NotOffered
        );
        // An answer replaces it.
        assert_eq!(
            gate.record_at(&me, Offer::NotOffered, t1),
            Offer::NotOffered
        );
        assert_eq!(gate.record_at(&off, Offer::Offered, t1), Offer::Offered);
    }

    #[test]
    fn the_answer_is_kept_per_caller_and_per_workspace() {
        let gate = QuestionsGate::new();
        let pilot = Uuid::new_v4();
        let other = Uuid::new_v4();
        gate.record(&key("me", pilot), Offer::Offered);
        assert_eq!(gate.fresh(&key("me", pilot)), Some(Offer::Offered));
        // The switch is per workspace: another workspace of the same caller is not known.
        assert_eq!(gate.fresh(&key("me", other)), None);
        // Nor is the same workspace for another caller.
        assert_eq!(gate.fresh(&key("someone-else", pilot)), None);
        gate.record(&key("me", other), Offer::NotOffered);
        assert_eq!(gate.fresh(&key("me", pilot)), Some(Offer::Offered));
        assert_eq!(gate.fresh(&key("me", other)), Some(Offer::NotOffered));
    }

    #[test]
    fn the_cache_stays_bounded_and_keeps_the_newest() {
        let gate = QuestionsGate::new();
        let workspace = Uuid::new_v4();
        let t0 = Instant::now();
        for i in 0..(MAX_ENTRIES + 25) {
            gate.record_at(
                &key(&format!("caller-{i}"), workspace),
                Offer::NotOffered,
                t0 + Duration::from_millis(i as u64),
            );
        }
        assert!(gate.cached_count() <= MAX_ENTRIES);
        let newest = key(&format!("caller-{}", MAX_ENTRIES + 24), workspace);
        assert_eq!(
            gate.fresh_at(&newest, t0 + Duration::from_secs(60)),
            Some(Offer::NotOffered)
        );
        assert_eq!(
            gate.fresh_at(&key("caller-0", workspace), t0 + Duration::from_secs(60)),
            None
        );
    }
}
