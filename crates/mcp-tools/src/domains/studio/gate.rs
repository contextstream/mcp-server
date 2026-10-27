//! Whether ContextStudio is offered to a caller, cached per caller.
//!
//! `studio` is listed only for a person Create's catalog offers one-prompt making to
//! (`GET /create/catalog` answers 200 with a `make` block); everyone else pays no prompt for
//! it. The offer depends only on who asks, so it is cached per caller identity:
//!
//! - offered or not (200, or 403/404 for an account Create isn't open to): 5 minutes;
//! - unreachable (network, timeout, 5xx): read again after 30 s, and an answer known before
//!   keeps being served meanwhile, so a blip never takes the tool away mid-session.
//!
//! Every `studio` call reads the catalog's answer through [`StudioGate::record`] as well,
//! so an account let into the pilot is noticed on its next call.

use mcp_client::ContextStreamClient;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) const KNOWN_TTL: Duration = Duration::from_secs(300);
pub(crate) const UNREACHABLE_RETRY: Duration = Duration::from_secs(30);
/// The pause before a listing reads an unreachable catalog again.
const RETRY_PAUSE: Duration = Duration::from_millis(400);
/// Callers remembered at once; beyond it the oldest answers go first.
pub(crate) const MAX_ENTRIES: usize = 4096;

/// What the catalog said to one caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Offer {
    /// One-prompt making is offered (it may be briefly unavailable).
    Offered,
    /// Create answered, without making for this caller.
    NotOffered,
    /// The catalog could not be read.
    Unreachable,
}

impl Offer {
    /// Classify one catalog read (status and body).
    pub(crate) fn from_answer(status: u16, data: &Value) -> Self {
        match status {
            200..=299 if data.get("make").is_some_and(Value::is_object) => Self::Offered,
            200..=299 | 401 | 403 | 404 => Self::NotOffered,
            _ => Self::Unreachable,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    offer: Offer,
    read_at: Instant,
    refresh_at: Instant,
}

/// The process-wide cache of who ContextStudio is offered to.
#[derive(Default)]
pub struct StudioGate {
    entries: Mutex<HashMap<String, Entry>>,
}

impl StudioGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The gate every registry shares (the hosted gateway builds a registry per request).
    pub fn global() -> Arc<Self> {
        static GATE: OnceLock<Arc<StudioGate>> = OnceLock::new();
        GATE.get_or_init(|| Arc::new(Self::new())).clone()
    }

    /// The cached answer for `identity`, when it is still fresh.
    pub(crate) fn fresh(&self, identity: &str) -> Option<Offer> {
        let now = Instant::now();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(identity)
            .filter(|entry| now < entry.refresh_at)
            .map(|entry| entry.offer.clone())
    }

    /// Store one answer. An unreachable catalog never replaces an answer read before.
    pub(crate) fn record(&self, identity: &str, offer: Offer) -> Offer {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let kept = match (&offer, entries.get(identity)) {
            (Offer::Unreachable, Some(previous)) if previous.offer != Offer::Unreachable => Entry {
                offer: previous.offer.clone(),
                read_at: previous.read_at,
                refresh_at: now + UNREACHABLE_RETRY,
            },
            (Offer::Unreachable, _) => Entry {
                offer: Offer::Unreachable,
                read_at: now,
                refresh_at: now + UNREACHABLE_RETRY,
            },
            _ => Entry {
                offer: offer.clone(),
                read_at: now,
                refresh_at: now + KNOWN_TTL,
            },
        };
        let answer = kept.offer.clone();
        entries.insert(identity.to_string(), kept);
        if entries.len() > MAX_ENTRIES {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.read_at)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest);
            }
        }
        answer
    }

    /// Whether `studio` is listed for the caller now: the cached answer when fresh, else the
    /// catalog read within `budget` (a read that does not finish counts as unreachable).
    pub(crate) async fn offered(
        &self,
        client: &ContextStreamClient,
        identity: &str,
        budget: Duration,
    ) -> bool {
        if let Some(offer) = self.fresh(identity) {
            return offer == Offer::Offered;
        }
        // A blip (a rolling deploy's 502, a cold connection) is read again once while the
        // budget lasts: a client that lists tools once a session would otherwise go
        // without the tool for the whole session.
        let started = std::time::Instant::now();
        let mut offer = Offer::Unreachable;
        for attempt in 0..2 {
            let left = budget.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            if attempt > 0 {
                tokio::time::sleep(RETRY_PAUSE.min(left)).await;
            }
            let left = budget.saturating_sub(started.elapsed());
            let read = tokio::time::timeout(
                left,
                client.studio_call("GET", "/create/catalog", None, None, left),
            )
            .await;
            offer = match read {
                Ok(Ok(answer)) => Offer::from_answer(answer.status, &answer.data),
                _ => Offer::Unreachable,
            };
            if offer != Offer::Unreachable {
                break;
            }
        }
        self.record(identity, offer) == Offer::Offered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_catalog_answer_decides_the_offer() {
        assert_eq!(
            Offer::from_answer(200, &json!({"make": {"status": "available"}})),
            Offer::Offered
        );
        assert_eq!(
            Offer::from_answer(200, &json!({"modes": []})),
            Offer::NotOffered
        );
        assert_eq!(Offer::from_answer(403, &json!(null)), Offer::NotOffered);
        assert_eq!(Offer::from_answer(502, &json!(null)), Offer::Unreachable);
    }

    #[test]
    fn a_blip_keeps_the_answer_known_before() {
        let gate = StudioGate::new();
        assert_eq!(gate.record("me", Offer::Offered), Offer::Offered);
        assert_eq!(gate.record("me", Offer::Unreachable), Offer::Offered);
        assert_eq!(gate.fresh("me"), Some(Offer::Offered));
        assert_eq!(gate.record("new", Offer::Unreachable), Offer::Unreachable);
        assert_eq!(gate.record("me", Offer::NotOffered), Offer::NotOffered);
    }
}
