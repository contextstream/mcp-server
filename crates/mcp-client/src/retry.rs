//! Retry logic with exponential backoff.

use mcp_types::Error;
use std::time::Duration;

/// Retry configuration.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retries
    pub max_retries: u32,
    /// Base delay between retries
    pub base_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Jitter factor (0.0 to 1.0)
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            jitter: 0.1,
        }
    }
}

impl RetryConfig {
    /// Calculate delay for a given attempt (0-indexed).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base = self.base_delay.as_millis() as f64;
        let exponential = base * 2_f64.powi(attempt as i32);
        let max = self.max_delay.as_millis() as f64;
        let delay = exponential.min(max);

        // Add jitter
        let jitter_range = delay * self.jitter;
        let jitter = (rand_simple() * 2.0 - 1.0) * jitter_range;
        let final_delay = (delay + jitter).max(0.0);

        Duration::from_millis(final_delay as u64)
    }

    /// Check if we should retry for the given error and attempt.
    pub fn should_retry(&self, error: &Error, attempt: u32) -> bool {
        if attempt >= self.max_retries {
            return false;
        }
        error.is_retryable()
    }

    /// How long to wait before retrying `error` inside the current request,
    /// or `None` when the server asked for a longer wait than a request
    /// should block for. The caller then gets the `RateLimited` error and
    /// schedules the retry itself (a sync loop must not sleep for minutes
    /// inside one HTTP call, nor ignore the server's instruction).
    pub fn delay_before_retry(&self, error: &Error, attempt: u32) -> Option<Duration> {
        match Self::retry_after(error) {
            Some(wait) if wait > self.max_delay => None,
            Some(wait) => Some(wait),
            None => Some(self.delay_for_attempt(attempt)),
        }
    }

    /// Get retry-after duration from error, if available.
    pub fn retry_after(error: &Error) -> Option<Duration> {
        match error {
            Error::RateLimited {
                retry_after: Some(secs),
                ..
            } => Some(Duration::from_secs(*secs)),
            _ => None,
        }
    }
}

/// How a caller that schedules its own ingest retries (the sync bridge, the
/// background index repair) should treat a failed ingest request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestFailure {
    /// 403: the caller has no write access to the project. Every retry gets
    /// the same answer until access, the checkout mapping or the credentials
    /// change.
    Denied,
    /// 401: the credentials were rejected; the user has to sign in again.
    Unauthorized,
    /// 429: the server asked for a pause, in seconds when it said how long.
    RateLimited { retry_after: Option<u64> },
    /// Timeouts, network failures, 5xx and anything else.
    Transient,
}

impl IngestFailure {
    pub fn classify(error: &Error) -> Self {
        match error {
            Error::Http { status: 403, .. } | Error::PlanRestriction(_) => Self::Denied,
            Error::Http { status: 401, .. } | Error::MissingCredentials => Self::Unauthorized,
            Error::RateLimited { retry_after, .. } => Self::RateLimited {
                retry_after: *retry_after,
            },
            Error::Http { status: 429, .. } => Self::RateLimited { retry_after: None },
            _ => Self::Transient,
        }
    }

    /// Whether the failure covers the whole project rather than one request,
    /// so the remaining batches of an upload would only repeat it.
    pub fn halts_project(self) -> bool {
        !matches!(self, Self::Transient)
    }
}

/// Simple pseudo-random number generator (for jitter).
/// Not cryptographically secure, but sufficient for retry jitter.
fn rand_simple() -> f64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    nanos as f64 / u32::MAX as f64
}

/// Retryable status codes.
pub const RETRYABLE_STATUSES: &[u16] = &[408, 429, 500, 502, 503, 504];

/// Check if an HTTP status is retryable.
pub fn is_retryable_status(status: u16) -> bool {
    RETRYABLE_STATUSES.contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_retry_after_is_returned_to_the_caller_instead_of_slept() {
        let config = RetryConfig::default();
        let rate_limited = |secs| Error::RateLimited {
            message: "slow down".into(),
            retry_after: Some(secs),
        };
        assert_eq!(
            config.delay_before_retry(&rate_limited(2), 0),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            config.delay_before_retry(&rate_limited(30), 0),
            Some(Duration::from_secs(30))
        );
        assert_eq!(config.delay_before_retry(&rate_limited(120), 0), None);
        // Without a server hint the usual exponential backoff applies.
        assert!(config
            .delay_before_retry(&Error::http(503, "unavailable"), 0)
            .is_some());
    }

    #[test]
    fn ingest_failures_are_classified_for_caller_scheduling() {
        assert_eq!(
            IngestFailure::classify(&Error::http(403, "no write role")),
            IngestFailure::Denied
        );
        assert_eq!(
            IngestFailure::classify(&Error::http(401, "expired")),
            IngestFailure::Unauthorized
        );
        assert_eq!(
            IngestFailure::classify(&Error::RateLimited {
                message: "hold".into(),
                retry_after: Some(120),
            }),
            IngestFailure::RateLimited {
                retry_after: Some(120)
            }
        );
        assert_eq!(
            IngestFailure::classify(&Error::http(429, "hold")),
            IngestFailure::RateLimited { retry_after: None }
        );
        for transient in [
            Error::http(503, "unavailable"),
            Error::Timeout(60),
            Error::Network("reset".into()),
        ] {
            let failure = IngestFailure::classify(&transient);
            assert_eq!(failure, IngestFailure::Transient);
            assert!(!failure.halts_project());
        }
        assert!(IngestFailure::Denied.halts_project());
        assert!(IngestFailure::Unauthorized.halts_project());
        assert!(IngestFailure::RateLimited { retry_after: None }.halts_project());
    }

    #[test]
    fn test_delay_calculation() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            jitter: 0.0, // No jitter for deterministic test
        };

        // Attempt 0: 1s * 2^0 = 1s
        let delay0 = config.delay_for_attempt(0);
        assert_eq!(delay0, Duration::from_secs(1));

        // Attempt 1: 1s * 2^1 = 2s
        let delay1 = config.delay_for_attempt(1);
        assert_eq!(delay1, Duration::from_secs(2));

        // Attempt 2: 1s * 2^2 = 4s
        let delay2 = config.delay_for_attempt(2);
        assert_eq!(delay2, Duration::from_secs(4));
    }

    #[test]
    fn test_max_delay_cap() {
        let config = RetryConfig {
            max_retries: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            jitter: 0.0,
        };

        // Attempt 5: 1s * 2^5 = 32s, capped to 10s
        let delay = config.delay_for_attempt(5);
        assert_eq!(delay, Duration::from_secs(10));
    }

    #[test]
    fn test_retryable_statuses() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
    }
}
