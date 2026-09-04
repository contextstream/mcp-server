//! Operator-controlled rollout. No client request or recalled memory can opt in.
//! Default/invalid configuration and the independent kill switch serve legacy.
use serde::Deserialize;

pub const POLICY_REVISION: &str = "grounding-evidence-v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    pub policy_revision: String,
    pub corpus_sha256: String,
    pub candidate_sha256: String,
    pub development_queries: u64,
    pub holdout_queries: u64,
    pub independent_labels: bool,
    pub coding_qualified: bool,
    pub evaluated_at_unix: i64,
    pub false_grounding_rate: f64,
    pub known_item_top1: f64,
    pub precision_at_5: f64,
    pub baseline_p95_ms: f64,
    pub candidate_p95_ms: f64,
    pub privacy_violations: u64,
    pub authority_violations: u64,
    pub false_completion_violations: u64,
}

impl Qualification {
    pub fn passes(&self, candidate: &str, corpus: &str) -> bool {
        let fraction = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
        self.policy_revision == POLICY_REVISION
            && sha256(candidate)
            && sha256(corpus)
            && self.candidate_sha256 == candidate
            && self.corpus_sha256 == corpus
            && self.development_queries == 60
            && self.holdout_queries == 60
            && self.independent_labels
            && self.coding_qualified
            && fraction(self.false_grounding_rate)
            && self.false_grounding_rate <= 0.05
            && fraction(self.known_item_top1)
            && self.known_item_top1 >= 0.95
            && fraction(self.precision_at_5)
            && self.precision_at_5 >= 0.8
            && self.baseline_p95_ms.is_finite()
            && self.baseline_p95_ms > 0.0
            && self.candidate_p95_ms.is_finite()
            && self.candidate_p95_ms > 0.0
            && self.candidate_p95_ms <= self.baseline_p95_ms * 1.10
            && self.privacy_violations == 0
            && self.authority_violations == 0
            && self.false_completion_violations == 0
    }
}

fn sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryEvidence {
    pub policy_revision: String,
    pub candidate_sha256: String,
    pub corpus_sha256: String,
    pub started_at_unix: i64,
    pub observed_until_unix: i64,
    pub eligible_operations: u64,
    pub privacy_violations: u64,
    pub authority_violations: u64,
    pub false_completion_violations: u64,
    pub baseline_p95_ms: f64,
    pub candidate_p95_ms: f64,
}

impl CanaryEvidence {
    fn passes(&self, now: i64, candidate: &str, corpus: &str) -> bool {
        self.policy_revision == POLICY_REVISION
            && self.candidate_sha256 == candidate
            && self.corpus_sha256 == corpus
            && self.started_at_unix > 0
            && self.observed_until_unix <= now
            && self
                .observed_until_unix
                .saturating_sub(self.started_at_unix)
                >= 48 * 60 * 60
            && self.eligible_operations >= 1000
            && self.privacy_violations == 0
            && self.authority_violations == 0
            && self.false_completion_violations == 0
            && self.baseline_p95_ms.is_finite()
            && self.baseline_p95_ms > 0.0
            && self.candidate_p95_ms.is_finite()
            && self.candidate_p95_ms > 0.0
            && self.candidate_p95_ms <= self.baseline_p95_ms * 1.10
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollout {
    pub phase: String,
    pub candidate_sha256: String,
    pub corpus_sha256: String,
    pub cohort_salt: String,
    #[serde(default)]
    pub internal_subjects: Vec<String>,
    pub qualification: Qualification,
    pub canary: Option<CanaryEvidence>,
}

impl Rollout {
    pub fn mode(
        &self,
        subject: Option<&str>,
        workspace: Option<&str>,
        project: Option<&str>,
        now: i64,
    ) -> &'static str {
        if !self
            .qualification
            .passes(&self.candidate_sha256, &self.corpus_sha256)
        {
            return "shadow";
        }
        if self.qualification.evaluated_at_unix <= 0
            || self.qualification.evaluated_at_unix > now
            || now.saturating_sub(self.qualification.evaluated_at_unix) > 7 * 24 * 60 * 60
        {
            return "shadow";
        }
        let (Some(subject), Some(workspace)) = (
            subject.filter(|s| !s.is_empty()),
            workspace.filter(|s| !s.is_empty()),
        ) else {
            return "shadow";
        };
        match self.phase.as_str() {
            "internal" if self.internal_subjects.iter().any(|s| s == subject) => "internal",
            "canary"
                if self.cohort_salt.len() >= 16
                    && cohort_bucket(&self.cohort_salt, subject, workspace, project) < 500 =>
            {
                "canary"
            }
            "general"
                if self.canary.as_ref().is_some_and(|c| {
                    c.passes(now, &self.candidate_sha256, &self.corpus_sha256)
                }) =>
            {
                "general"
            }
            _ => "shadow",
        }
    }
}

// Versioned FNV-1a with length-framed fields, stable across processes/platforms.
// This is sampling, not authentication. Subjects come from authenticated scope.
fn cohort_bucket(salt: &str, subject: &str, workspace: &str, project: Option<&str>) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for field in [
        POLICY_REVISION,
        salt,
        subject,
        workspace,
        project.unwrap_or(""),
    ] {
        for byte in (field.len() as u64)
            .to_le_bytes()
            .iter()
            .chain(field.as_bytes())
        {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
    hash % 10_000
}

pub fn configured() -> bool {
    std::env::var_os("CONTEXTSTREAM_GROUNDING_ROLLOUT").is_some()
        || std::env::var_os("CONTEXTSTREAM_GROUNDING_FORCE_SHADOW").is_some()
}

pub fn serving_mode(
    subject: Option<&str>,
    workspace: Option<&str>,
    project: Option<&str>,
) -> &'static str {
    if force_shadow(
        std::env::var("CONTEXTSTREAM_GROUNDING_FORCE_SHADOW")
            .ok()
            .as_deref(),
    ) {
        return "shadow";
    }
    std::env::var("CONTEXTSTREAM_GROUNDING_ROLLOUT")
        .ok()
        .filter(|v| v.len() <= 64 * 1024)
        .and_then(|v| serde_json::from_str::<Rollout>(&v).ok())
        .map(|r| r.mode(subject, workspace, project, chrono::Utc::now().timestamp()))
        .unwrap_or("shadow")
}

fn force_shadow(value: Option<&str>) -> bool {
    // Malformed kill-switch configuration is fail-closed, not an opt-in.
    value.is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kill_switch_is_case_insensitive_and_malformed_values_fail_closed() {
        for value in ["1", "true", "TRUE", "on", "unexpected", ""] {
            assert!(force_shadow(Some(value)));
        }
        for value in ["0", "false", "OFF"] {
            assert!(!force_shadow(Some(value)));
        }
        assert!(!force_shadow(None));
    }
    fn config() -> Rollout {
        serde_json::from_value(serde_json::json!({
            "phase":"internal", "candidate_sha256":"a".repeat(64), "corpus_sha256":"b".repeat(64),
            "cohort_salt":"fixed-sampling-salt", "internal_subjects":["internal"],
            "qualification": {"policy_revision":POLICY_REVISION, "candidate_sha256":"a".repeat(64), "corpus_sha256":"b".repeat(64),
                "development_queries":60,"holdout_queries":60,"independent_labels":true,"coding_qualified":true,"evaluated_at_unix":1,
                "false_grounding_rate":0.05,"known_item_top1":0.95,"precision_at_5":0.8,
                "baseline_p95_ms":100.0,"candidate_p95_ms":110.0,
                "privacy_violations":0,"authority_violations":0,"false_completion_violations":0}
        })).unwrap()
    }
    #[test]
    fn internal_requires_explicit_subject_and_qualification() {
        let mut r = config();
        assert_eq!(r.mode(Some("internal"), Some("ws"), None, 1), "internal");
        assert_eq!(r.mode(Some("external"), Some("ws"), None, 1), "shadow");
        assert_eq!(r.mode(None, Some("ws"), None, 1), "shadow");
        r.qualification.independent_labels = false;
        assert_eq!(r.mode(Some("internal"), Some("ws"), None, 1), "shadow");
        r.qualification.independent_labels = true;
        r.qualification.candidate_p95_ms = f64::NAN;
        assert_eq!(r.mode(Some("internal"), Some("ws"), None, 1), "shadow");
    }
    #[test]
    fn five_percent_is_sticky_and_scope_framed() {
        let mut r = config();
        r.phase = "canary".into();
        let selected = (0..10_000)
            .filter(|i| r.mode(Some(&i.to_string()), Some("ws"), Some("project"), 1) == "canary")
            .count();
        assert!((400..600).contains(&selected), "{selected}");
        assert_eq!(
            cohort_bucket("salt", "one", "ws", None),
            cohort_bucket("salt", "one", "ws", None)
        );
        assert_ne!(
            cohort_bucket("salt", "ab", "c", None),
            cohort_bucket("salt", "a", "bc", None)
        );
    }
    #[test]
    fn expansion_needs_time_and_volume_and_clean_safety() {
        let mut r = config();
        r.phase = "general".into();
        assert_eq!(r.mode(Some("u"), Some("ws"), None, 200_000), "shadow");
        r.canary = Some(CanaryEvidence {
            policy_revision: POLICY_REVISION.into(),
            candidate_sha256: "a".repeat(64),
            corpus_sha256: "b".repeat(64),
            started_at_unix: 1,
            observed_until_unix: 172_801,
            eligible_operations: 1000,
            privacy_violations: 0,
            authority_violations: 0,
            false_completion_violations: 0,
            baseline_p95_ms: 100.0,
            candidate_p95_ms: 110.0,
        });
        assert_eq!(r.mode(Some("u"), Some("ws"), None, 200_000), "general");
        r.canary.as_mut().unwrap().eligible_operations = 999;
        assert_eq!(r.mode(Some("u"), Some("ws"), None, 200_000), "shadow");
        r.canary.as_mut().unwrap().eligible_operations = 1000;
        r.canary.as_mut().unwrap().observed_until_unix = 172_800;
        assert_eq!(r.mode(Some("u"), Some("ws"), None, 200_000), "shadow");
        r.canary.as_mut().unwrap().observed_until_unix = 172_801;
        r.canary.as_mut().unwrap().authority_violations = 1;
        assert_eq!(r.mode(Some("u"), Some("ws"), None, 200_000), "shadow");
    }
}
