//! Pure decision logic for the daemon's daily database health routine
//! (`daemon::health_routine`): the recorded status shape and the cadence
//! decision ("is it due yet"). No I/O — the daemon layer gathers facts
//! (integrity results, disk space, busy state) and persists this status via
//! `StateRepository`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The `daemon_state` key the routine's last-known status is persisted
/// under (see `StateRepository::get_state`/`set_state`).
pub const STATE_KEY: &str = "db_health_status";

/// The daily cadence (decision 1 in the health-routine spec): a run is due
/// once this many seconds have passed since the last one.
pub const HEALTH_CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// What the most recent completed run found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbHealthOutcome {
    /// `integrity_check` was `ok` and the backup was written and verified.
    Passed,
    /// `integrity_check` found a problem — no backup was attempted
    /// (decision: a backup taken from a corrupt source would overwrite the
    /// last good one).
    IntegrityFailed,
    /// The source was healthy, but the newly written backup failed its own
    /// `integrity_check` (or the write/rename itself failed) — discarded,
    /// previous backup left in place.
    BackupVerificationFailed,
    /// The source was healthy, but there wasn't enough free disk space to
    /// safely attempt a `VACUUM INTO` copy — skipped, previous backup left
    /// in place.
    BackupSkippedInsufficientDiskSpace,
}

/// Persisted record of the health routine's state, round-tripped through
/// `daemon_state` as JSON under [`STATE_KEY`]. Every field is optional/empty
/// by default so a fresh install (never run) round-trips to "never run"
/// rather than needing a sentinel value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DbHealthStatus {
    /// When the routine last actually ran to completion (as opposed to
    /// being skipped for busy-ness). `None` means "never run" — distinct
    /// from `Some(outcome) == Passed`, which means "ran and passed".
    pub last_run_at: Option<DateTime<Utc>>,
    pub outcome: Option<DbHealthOutcome>,
    /// The raw `integrity_check` verdict from the last completed run
    /// (`"ok"` or the problem rows it reported).
    pub integrity_result: Option<String>,
    /// `foreign_key_check` violations from the last completed run. Always
    /// reported, never fatal (decision 3).
    pub foreign_key_violations: Vec<String>,
    /// Where the verified backup lives, once one has ever been written.
    pub backup_path: Option<String>,
    /// The most recent time the routine was due but skipped because the
    /// daemon wasn't idle.
    pub last_skip_at: Option<DateTime<Utc>>,
    /// Why it was skipped (the busy reasons, e.g. "1 loop(s) currently
    /// running").
    pub last_skip_reason: Option<String>,
}

impl DbHealthStatus {
    pub fn from_json(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        // A `Vec`/`Option`-only struct always serializes; a failure here
        // would be a programmer error (e.g. a non-finite float), not a
        // runtime condition to recover from.
        serde_json::to_string(self).expect("DbHealthStatus always serializes")
    }
}

/// Is a run due? `None` (never run) is always due. Otherwise, due once
/// [`HEALTH_CHECK_INTERVAL_SECS`] have elapsed since `last_run_at`.
///
/// Note this only looks at `last_run_at`, not `last_skip_at` — a run that
/// was skipped for busy-ness stays due (and is re-evaluated on the next
/// poll) rather than waiting out a fresh 24h window from the skip.
pub fn is_due(last_run_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last_run_at {
        None => true,
        Some(t) => now.signed_duration_since(t).num_seconds() >= HEALTH_CHECK_INTERVAL_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_due_when_never_run() {
        assert!(is_due(None, Utc::now()));
    }

    #[test]
    fn is_due_false_just_after_a_run() {
        let now = Utc::now();
        assert!(!is_due(Some(now), now));
    }

    #[test]
    fn is_due_false_shortly_before_the_interval_elapses() {
        let now = Utc::now();
        let last_run = now - chrono::Duration::seconds(HEALTH_CHECK_INTERVAL_SECS - 1);
        assert!(!is_due(Some(last_run), now));
    }

    #[test]
    fn is_due_true_once_the_interval_has_elapsed() {
        let now = Utc::now();
        let last_run = now - chrono::Duration::seconds(HEALTH_CHECK_INTERVAL_SECS);
        assert!(is_due(Some(last_run), now));
    }

    #[test]
    fn is_due_true_well_past_the_interval() {
        let now = Utc::now();
        let last_run = now - chrono::Duration::days(30);
        assert!(is_due(Some(last_run), now));
    }

    #[test]
    fn status_json_round_trips() {
        let status = DbHealthStatus {
            last_run_at: Some(Utc::now()),
            outcome: Some(DbHealthOutcome::Passed),
            integrity_result: Some("ok".to_string()),
            foreign_key_violations: vec!["loop_specs rowid=1 -> loops".to_string()],
            backup_path: Some("/home/user/.canopy/background_agents.db.backup".to_string()),
            last_skip_at: None,
            last_skip_reason: None,
        };
        let json = status.to_json();
        let round_tripped = DbHealthStatus::from_json(&json);
        assert_eq!(round_tripped.outcome, Some(DbHealthOutcome::Passed));
        assert_eq!(round_tripped.integrity_result, status.integrity_result);
        assert_eq!(
            round_tripped.foreign_key_violations,
            status.foreign_key_violations
        );
        assert_eq!(round_tripped.backup_path, status.backup_path);
    }

    #[test]
    fn status_from_json_defaults_on_garbage() {
        let status = DbHealthStatus::from_json("not json");
        assert!(status.last_run_at.is_none());
        assert!(status.outcome.is_none());
    }

    #[test]
    fn status_from_json_defaults_on_missing_key() {
        // Simulates a fresh install: `get_state` returns `None`, which the
        // caller maps to an empty string / never calls this with — but an
        // empty-object JSON (e.g. from a future schema) must still parse to
        // the "never run" default rather than erroring.
        let status = DbHealthStatus::from_json("{}");
        assert!(status.last_run_at.is_none());
        assert!(status.outcome.is_none());
        assert!(status.foreign_key_violations.is_empty());
    }
}
