//! Durable "this project's payer is out of credits" record.
//!
//! A refused ingest used to be retried: the API answered 402 (or a queued job
//! failed for credits) and every automatic sender, the sync bridge, the hook
//! drain and the search preflight, sent the same files again a few seconds
//! later. On 2026-10-27 one workspace sent 1,358 jobs in 45 minutes that way,
//! and nobody was told.
//!
//! This module is the one place that remembers the refusal, so all of them stop
//! together even though they are separate processes:
//!
//! * the first credit refusal starts a hold for the project; automatic ingest
//!   requests for it are answered locally, without a request, until the hold's
//!   recheck time (5 minutes, then 10, 20 and 30);
//! * a recheck that is accepted clears the hold, so indexing resumes by itself
//!   once credits are added;
//! * a hold carries a notice that a hook shows the person once
//!   ([`take_unshown_notice`]), however many times the refusal repeats.
//!
//! The dirty-file ledger is not touched: the files stay queued and are sent
//! when the hold ends.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CREDIT_HOLDS_FILE: &str = "ingest-credit-holds.json";

/// Recheck delays by consecutive refusal (1st, 2nd, 3rd, 4th and later).
const RECHECK_DELAYS: [Duration; 4] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(20 * 60),
    Duration::from_secs(30 * 60),
];

/// A hold left on disk longer than this without a refusal is stale (the
/// machine was off, the process died) and is ignored and dropped.
const HOLD_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub const PRICING_URL: &str = "https://contextstream.io/pricing";
pub const BILLING_URL: &str = "https://contextstream.io/account/billing";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditHold {
    pub project_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    /// When the hold began (its first refusal).
    pub since: DateTime<Utc>,
    pub last_refused_at: DateTime<Utc>,
    /// No automatic ingest request is sent for the project before this.
    pub retry_after: DateTime<Utc>,
    /// Consecutive refusals; drives the recheck delay.
    pub streak: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<i32>,
    /// When a person was first told about this hold. A hold is told once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CreditHoldFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    holds: Vec<CreditHold>,
}

/// How long to wait before the next try after the `streak`-th refusal.
pub fn recheck_delay(streak: u32) -> Duration {
    let index = (streak.max(1) as usize - 1).min(RECHECK_DELAYS.len() - 1);
    RECHECK_DELAYS[index]
}

/// `~/.contextstream`, where the other machine-local ingest state lives.
pub fn default_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".contextstream"))
}

fn file_path(dir: &Path) -> PathBuf {
    dir.join(CREDIT_HOLDS_FILE)
}

fn read_holds(dir: &Path) -> CreditHoldFile {
    std::fs::read_to_string(file_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_holds(dir: &Path, file: &CreditHoldFile) -> std::io::Result<()> {
    use std::io::Write as _;

    let path = file_path(dir);
    if file.holds.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(dir)?;
    let temporary = dir.join(format!(
        ".{CREDIT_HOLDS_FILE}.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec(file).map_err(std::io::Error::other)?;
    let result = (|| {
        let mut handle = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        handle.write_all(&bytes)?;
        handle.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Run `operation` on the holds file under an exclusive lock, writing back what
/// it changed. Best effort: when the state directory cannot be locked or
/// written, ingest behaves as before (it is not held).
fn with_holds<T>(
    dir: &Path,
    now: DateTime<Utc>,
    operation: impl FnOnce(&mut CreditHoldFile) -> T,
) -> Option<T> {
    use fs2::FileExt as _;

    std::fs::create_dir_all(dir).ok()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join(format!(".{CREDIT_HOLDS_FILE}.lock")))
        .ok()?;
    lock.lock_exclusive().ok()?;
    let mut file = read_holds(dir);
    let before = serde_json::to_vec(&file).ok();
    let max_age =
        chrono::Duration::from_std(HOLD_MAX_AGE).unwrap_or_else(|_| chrono::Duration::days(1));
    file.holds
        .retain(|hold| now.signed_duration_since(hold.last_refused_at) < max_age);
    let result = operation(&mut file);
    file.version = 1;
    if serde_json::to_vec(&file).ok() != before {
        let _ = write_holds(dir, &file);
    }
    let _ = fs2::FileExt::unlock(&lock);
    Some(result)
}

/// Record one credit refusal for `project_id` and start or extend its hold.
/// Returns the hold, and whether it is a new one (its first refusal).
pub fn record_refusal_at(
    dir: &Path,
    project_id: Uuid,
    workspace_id: Option<Uuid>,
    required: Option<i32>,
    available: Option<i32>,
    now: DateTime<Utc>,
) -> Option<(CreditHold, bool)> {
    with_holds(dir, now, |file| {
        let existing = file
            .holds
            .iter()
            .position(|hold| hold.project_id == project_id);
        let (previous, fresh) = match existing {
            Some(index) => (file.holds.remove(index), false),
            None => (
                CreditHold {
                    project_id,
                    workspace_id,
                    since: now,
                    last_refused_at: now,
                    retry_after: now,
                    streak: 0,
                    required,
                    available,
                    notified_at: None,
                },
                true,
            ),
        };
        let streak = previous.streak.saturating_add(1);
        let delay = chrono::Duration::from_std(recheck_delay(streak))
            .unwrap_or_else(|_| chrono::Duration::minutes(30));
        let hold = CreditHold {
            workspace_id: workspace_id.or(previous.workspace_id),
            last_refused_at: now,
            retry_after: now + delay,
            streak,
            required: required.or(previous.required),
            available: available.or(previous.available),
            ..previous
        };
        file.holds.push(hold.clone());
        (hold, fresh)
    })
}

pub fn record_refusal(
    project_id: Uuid,
    workspace_id: Option<Uuid>,
    required: Option<i32>,
    available: Option<i32>,
) -> Option<(CreditHold, bool)> {
    record_refusal_at(
        &default_dir()?,
        project_id,
        workspace_id,
        required,
        available,
        Utc::now(),
    )
}

/// The project's hold while automatic ingest must not be sent (its recheck time
/// has not come). Read-only and cheap: one small file.
pub fn active_hold_at(dir: &Path, project_id: Uuid, now: DateTime<Utc>) -> Option<CreditHold> {
    let max_age =
        chrono::Duration::from_std(HOLD_MAX_AGE).unwrap_or_else(|_| chrono::Duration::days(1));
    read_holds(dir).holds.into_iter().find(|hold| {
        hold.project_id == project_id
            && hold.retry_after > now
            && now.signed_duration_since(hold.last_refused_at) < max_age
    })
}

pub fn active_hold(project_id: Uuid) -> Option<CreditHold> {
    active_hold_at(&default_dir()?, project_id, Utc::now())
}

/// Whether any project on this machine has a hold on record. One `stat`, for
/// hot paths that only need to know there is nothing to look at.
pub fn any_holds() -> bool {
    default_dir().is_some_and(|dir| file_path(&dir).exists())
}

/// End the project's hold: an ingest was accepted, so its payer can pay again.
/// Returns whether there was a hold.
pub fn clear_at(dir: &Path, project_id: Uuid) -> bool {
    // Cheap miss on the common path: no holds file, no lock.
    if !file_path(dir).exists() {
        return false;
    }
    with_holds(dir, Utc::now(), |file| {
        let before = file.holds.len();
        file.holds.retain(|hold| hold.project_id != project_id);
        file.holds.len() != before
    })
    .unwrap_or(false)
}

pub fn clear(project_id: Uuid) -> bool {
    default_dir().is_some_and(|dir| clear_at(&dir, project_id))
}

/// The hold to tell a person about, once: the project's hold when it has not
/// been told yet (`project_id` given), marked told so no other prompt or session
/// repeats it. A hold that is cleared and later starts again is told again.
pub fn take_unshown_notice_at(
    dir: &Path,
    project_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Option<CreditHold> {
    let project_id = project_id?;
    if !file_path(dir).exists() {
        return None;
    }
    with_holds(dir, now, |file| {
        let hold = file
            .holds
            .iter_mut()
            .find(|hold| hold.project_id == project_id && hold.notified_at.is_none())?;
        hold.notified_at = Some(now);
        Some(hold.clone())
    })
    .flatten()
}

pub fn take_unshown_notice(project_id: Option<Uuid>) -> Option<CreditHold> {
    take_unshown_notice_at(&default_dir()?, project_id, Utc::now())
}

/// What the person is told, in plain words.
pub fn notice_text(hold: &CreditHold) -> String {
    let balance = match (hold.required, hold.available) {
        (Some(required), Some(available)) => {
            format!(" The last upload needed {required} credits and {available} are available.")
        }
        _ => String::new(),
    };
    format!(
        "ContextStream indexing for this project is paused because your account is out of credits.{balance} \
         Your edits are kept on this machine and nothing is lost. To continue, add credits or upgrade \
         ({PRICING_URL}; billing: {BILLING_URL}). Indexing resumes by itself within about \
         {} minutes of credits being added.",
        recheck_delay(hold.streak.max(1)).as_secs().div_ceil(60).max(5)
    )
}

/// Pull the amounts out of the worker's failure text:
/// `insufficient credits for raw ingest: required=20, available=3`, possibly
/// behind a `batch <id> (+N jobs): job <id>: ` prefix.
pub fn parse_credit_amounts(message: &str) -> (Option<i32>, Option<i32>) {
    let amount = |key: &str| {
        let start = message.find(key)? + key.len();
        let digits: String = message[start..]
            .chars()
            .take_while(|character| character.is_ascii_digit() || *character == '-')
            .collect();
        digits.parse::<i32>().ok()
    };
    (amount("required="), amount("available="))
}

/// Whether a job's progress JSON says it failed because the payer could not
/// pay. The API sets `error_code: "INSUFFICIENT_CREDITS"`; the message is read
/// too, so servers that only send `error_message` are understood.
pub fn job_failed_for_credits(progress: &serde_json::Value) -> bool {
    if progress
        .get("error_code")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|code| code.trim().eq_ignore_ascii_case("INSUFFICIENT_CREDITS"))
    {
        return true;
    }
    progress
        .get("error_message")
        .or_else(|| progress.get("error"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|message| {
            message
                .to_ascii_lowercase()
                .contains("insufficient credits")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn recheck_delay_grows_to_thirty_minutes() {
        let minutes = |streak| recheck_delay(streak).as_secs() / 60;
        assert_eq!([1, 2, 3, 4, 9, 400].map(minutes), [5, 10, 20, 30, 30, 30]);
        assert_eq!(minutes(0), 5, "a zero streak is the first refusal");
    }

    #[test]
    fn a_refusal_holds_the_project_until_its_recheck_and_escalates() {
        let dir = tempfile::tempdir().unwrap();
        let project = Uuid::new_v4();
        let t0 = ts("2026-10-28T10:00:00Z");

        let (hold, fresh) =
            record_refusal_at(dir.path(), project, None, Some(20), Some(3), t0).unwrap();
        assert!(fresh);
        assert_eq!(
            (hold.streak, hold.required, hold.available),
            (1, Some(20), Some(3))
        );
        assert_eq!(hold.retry_after, t0 + chrono::Duration::minutes(5));

        // Held until the recheck time, free after it, for that project only.
        assert!(active_hold_at(dir.path(), project, t0 + chrono::Duration::minutes(4)).is_some());
        assert!(active_hold_at(dir.path(), project, t0 + chrono::Duration::minutes(5)).is_none());
        assert!(active_hold_at(dir.path(), Uuid::new_v4(), t0).is_none());

        // A failed recheck extends the same hold with a longer wait.
        let t1 = t0 + chrono::Duration::minutes(5);
        let (again, fresh) =
            record_refusal_at(dir.path(), project, None, None, Some(1), t1).unwrap();
        assert!(!fresh);
        assert_eq!(again.since, t0);
        assert_eq!(again.streak, 2);
        assert_eq!(again.retry_after, t1 + chrono::Duration::minutes(10));
        assert_eq!(
            again.required,
            Some(20),
            "an unknown amount keeps the last known one"
        );
        assert_eq!(again.available, Some(1));
    }

    #[test]
    fn an_accepted_ingest_clears_the_hold() {
        let dir = tempfile::tempdir().unwrap();
        let project = Uuid::new_v4();
        assert!(!clear_at(dir.path(), project), "nothing to clear");
        record_refusal_at(dir.path(), project, None, None, None, Utc::now()).unwrap();
        assert!(dir.path().join(CREDIT_HOLDS_FILE).exists());
        assert!(clear_at(dir.path(), project));
        assert!(active_hold_at(dir.path(), project, Utc::now()).is_none());
        assert!(
            !dir.path().join(CREDIT_HOLDS_FILE).exists(),
            "the file goes with its last hold"
        );
    }

    #[test]
    fn the_person_is_told_once_per_hold_and_again_after_it_clears() {
        let dir = tempfile::tempdir().unwrap();
        let project = Uuid::new_v4();
        let now = Utc::now();
        assert!(take_unshown_notice_at(dir.path(), Some(project), now).is_none());

        record_refusal_at(dir.path(), project, None, Some(20), Some(3), now).unwrap();
        assert!(
            take_unshown_notice_at(dir.path(), None, now).is_none(),
            "no project, no notice"
        );
        let hold = take_unshown_notice_at(dir.path(), Some(project), now).expect("first prompt");
        assert_eq!(hold.available, Some(3));
        assert!(
            take_unshown_notice_at(dir.path(), Some(project), now).is_none(),
            "not again on the next prompt"
        );

        // Refusals that repeat inside the hold are not new news.
        record_refusal_at(dir.path(), project, None, Some(20), Some(3), now).unwrap();
        assert!(take_unshown_notice_at(dir.path(), Some(project), now).is_none());

        // A new hold, after the old one cleared, is told again.
        assert!(clear_at(dir.path(), project));
        record_refusal_at(dir.path(), project, None, Some(20), Some(0), now).unwrap();
        assert!(take_unshown_notice_at(dir.path(), Some(project), now).is_some());
    }

    #[test]
    fn a_stale_hold_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let project = Uuid::new_v4();
        let t0 = ts("2026-10-01T00:00:00Z");
        record_refusal_at(dir.path(), project, None, None, None, t0).unwrap();
        let later = t0 + chrono::Duration::days(2);
        assert!(active_hold_at(dir.path(), project, later).is_none());
        // The next write drops it, so a refusal now is a new hold.
        let (_, fresh) = record_refusal_at(dir.path(), project, None, None, None, later).unwrap();
        assert!(fresh);
    }

    #[test]
    fn the_notice_says_what_happened_and_how_to_continue() {
        let hold = CreditHold {
            project_id: Uuid::new_v4(),
            workspace_id: None,
            since: Utc::now(),
            last_refused_at: Utc::now(),
            retry_after: Utc::now(),
            streak: 1,
            required: Some(20),
            available: Some(3),
            notified_at: None,
        };
        let text = notice_text(&hold);
        assert!(
            text.contains("paused because your account is out of credits"),
            "{text}"
        );
        assert!(
            text.contains("needed 20 credits and 3 are available"),
            "{text}"
        );
        assert!(text.contains("nothing is lost"), "{text}");
        assert!(
            text.contains(PRICING_URL) && text.contains(BILLING_URL),
            "{text}"
        );
        assert!(text.contains("resumes by itself"), "{text}");
        let unknown = CreditHold {
            required: None,
            available: None,
            ..hold
        };
        assert!(!notice_text(&unknown).contains("needed"));
    }

    #[test]
    fn credit_amounts_are_read_from_the_worker_failure_text() {
        let plain = "insufficient credits for raw ingest: required=20, available=3";
        assert_eq!(parse_credit_amounts(plain), (Some(20), Some(3)));
        let batch = "batch 6b0e (+12 jobs): job 1f2a: insufficient credits for raw ingest: required=20, available=14";
        assert_eq!(parse_credit_amounts(batch), (Some(20), Some(14)));
        assert_eq!(parse_credit_amounts("R2 fetch failed"), (None, None));
    }

    #[test]
    fn a_failed_job_is_a_credit_failure_by_code_or_by_message() {
        assert!(job_failed_for_credits(&json!({
            "status": "failed",
            "error_code": "INSUFFICIENT_CREDITS",
            "error_message": "anything"
        })));
        assert!(job_failed_for_credits(&json!({
            "status": "failed",
            "error_message": "batch 6b0e (+1 jobs): job 1f2a: insufficient credits for raw ingest: required=20, available=3"
        })));
        assert!(!job_failed_for_credits(&json!({
            "status": "failed",
            "error_code": "INTERNAL",
            "error_message": "R2 fetch failed: service error"
        })));
        assert!(!job_failed_for_credits(&json!({"status": "completed"})));
    }
}
