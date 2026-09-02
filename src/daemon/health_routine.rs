//! Daily database health routine: full `integrity_check` and
//! `foreign_key_check`, plus a verified single-backup replacement via
//! `VACUUM INTO`, run inside the daemon only while it's idle (no loop
//! running, no TUI attached).
//!
//! Reuses the same `tokio::spawn` + interval-poll pattern the internal cron
//! scheduler (`scheduler::cron_scheduler`) and the RAG ingestion manager
//! (`rag::ingestion::IngestionManager`) already use for their own periodic
//! background work — no new scheduler dependency. [`HealthRoutine::start`]
//! is started and cancelled from `daemon::server::run_http_server` exactly
//! like those.
//!
//! Split in two on purpose:
//! - [`run_health_check`] is the whole check-and-backup as one plain sync
//!   function over a `Database` and two paths. No idle/cadence decision, no
//!   tokio — directly callable from tests and from the on-demand trigger
//!   (`canopy daemon health-check`, see `daemon::cli`) without spinning up
//!   the daemon's background loop.
//! - [`HealthRoutine`] is the daemon-side wrapper: decides *when* to call
//!   it (idle + due), runs it off the async runtime via `spawn_blocking` (a
//!   long `integrity_check` on a large database must not stall loop
//!   execution or MCP requests), and persists the result.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::application::ports::StateRepository;
use crate::db::health::integrity_check_file;
use crate::db::Database;
use crate::domain::db_health::{is_due, DbHealthOutcome, DbHealthStatus, STATE_KEY};
use crate::domain::db_paths::{database_path, DB_FILE_NAME};

/// Where the single verified backup lives, under the canopy data directory.
pub fn backup_path(data_dir: &Path) -> PathBuf {
    data_dir.join(format!("{DB_FILE_NAME}.backup"))
}

const TEMP_SUFFIX: &str = ".tmp";

/// How often the daemon rechecks whether the routine is due and the daemon
/// is idle. Independent of the 24h cadence itself — a shorter poll just
/// means a busy daemon notices the moment it goes idle sooner, and a skip
/// gets re-evaluated (and re-recorded) promptly rather than staying
/// invisible for hours.
const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Safety margin over the live database's file size required as free disk
/// space before attempting `VACUUM INTO`: the copy is roughly the same
/// size (usually a little smaller, since it's compacted), so 10% headroom
/// absorbs that without risking a mid-copy `SQLITE_FULL`.
const DISK_SPACE_MARGIN_NUMERATOR: u64 = 11;
const DISK_SPACE_MARGIN_DENOMINATOR: u64 = 10;

/// The daemon-side wrapper around [`run_health_check`]: decides when to run
/// it (idle + due — decision 1 in the health-routine spec) and persists the
/// result.
pub struct HealthRoutine {
    db: Arc<Database>,
    data_dir: PathBuf,
}

impl HealthRoutine {
    pub fn new(db: Arc<Database>, data_dir: PathBuf) -> Self {
        Self { db, data_dir }
    }

    /// Start the routine as a background tokio task. Returns a
    /// `CancellationToken` the caller cancels on daemon shutdown.
    pub fn start(self: Arc<Self>) -> CancellationToken {
        let cancel = CancellationToken::new();
        let cancel_run = cancel.clone();
        let routine = Arc::clone(&self);
        tokio::spawn(async move {
            tracing::info!("Database health routine started");
            routine.run_loop(cancel_run).await;
            tracing::info!("Database health routine stopped");
        });
        cancel
    }

    async fn run_loop(&self, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(POLL_INTERVAL) => {
                    self.maybe_run().await;
                }
            }
        }
    }

    /// Runs the routine if (and only if) it's due and the daemon is
    /// currently idle. If it's due but busy, records the skip (so a
    /// routine that never finds an idle moment is visible) and leaves it
    /// due for the next poll rather than resetting the cadence.
    async fn maybe_run(&self) {
        let status = load_status(&self.db);
        let now = Utc::now();
        if !is_due(status.last_run_at, now) {
            return;
        }

        let busy = match self.db.busy_reasons() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("db health routine: could not check busy state: {e}");
                return;
            }
        };
        if !busy.is_empty() {
            let skipped = DbHealthStatus {
                last_skip_at: Some(now),
                last_skip_reason: Some(busy.join(", ")),
                ..status
            };
            tracing::info!("db health routine: deferred — busy: {}", busy.join(", "));
            if let Err(e) = save_status(&self.db, &skipped) {
                tracing::error!("db health routine: failed to record skip: {e}");
            }
            return;
        }

        let db = Arc::clone(&self.db);
        let db_path = database_path(&self.data_dir);
        let backup_path = backup_path(&self.data_dir);
        let new_status = match tokio::task::spawn_blocking(move || {
            run_health_check(&db, &db_path, &backup_path)
        })
        .await
        {
            Ok(status) => status,
            Err(e) => {
                tracing::error!("db health routine: task panicked: {e}");
                return;
            }
        };

        match new_status.outcome {
            Some(DbHealthOutcome::Passed) => {
                tracing::info!("db health routine: passed — backup written and verified");
            }
            ref other => {
                tracing::warn!("db health routine: finished with outcome {:?}", other);
            }
        }
        if let Err(e) = save_status(&self.db, &new_status) {
            tracing::error!("db health routine: failed to persist status: {e}");
        }
    }
}

/// Loads the routine's persisted status, defaulting to "never run" when
/// nothing has been recorded yet (fresh install, or a database predating
/// this routine).
pub fn load_status(db: &Database) -> DbHealthStatus {
    db.get_state(STATE_KEY)
        .ok()
        .flatten()
        .map(|s| DbHealthStatus::from_json(&s))
        .unwrap_or_default()
}

pub fn save_status(db: &Database, status: &DbHealthStatus) -> anyhow::Result<()> {
    db.set_state(STATE_KEY, &status.to_json())
}

/// The whole check-and-backup: full `integrity_check`, `foreign_key_check`,
/// and — only if integrity passed — a verified single-backup replacement.
/// Plain sync function over a `Database` and two paths; no idle/cadence
/// decision here (that's [`HealthRoutine::maybe_run`]'s job), which is what
/// keeps this directly callable from tests and from an on-demand trigger.
pub fn run_health_check(db: &Database, db_path: &Path, backup_path: &Path) -> DbHealthStatus {
    let now = Utc::now();

    let integrity_result = match db.integrity_check() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("db health routine: integrity_check errored: {e}");
            return DbHealthStatus {
                last_run_at: Some(now),
                outcome: Some(DbHealthOutcome::IntegrityFailed),
                integrity_result: Some(format!("integrity_check errored: {e}")),
                ..Default::default()
            };
        }
    };

    let foreign_key_violations = db
        .foreign_key_check()
        .unwrap_or_else(|e| vec![format!("foreign_key_check errored: {e}")]);

    if integrity_result != "ok" {
        // A backup taken from a corrupt source would overwrite the last
        // good one — report and stop, no backup attempt.
        tracing::error!("db health routine: integrity_check found problems: {integrity_result}");
        return DbHealthStatus {
            last_run_at: Some(now),
            outcome: Some(DbHealthOutcome::IntegrityFailed),
            integrity_result: Some(integrity_result),
            foreign_key_violations,
            ..Default::default()
        };
    }

    let temp_path = temp_backup_path(backup_path);
    // Best-effort cleanup of a temp file abandoned by a prior run that
    // never reached its rename (e.g. the daemon was killed mid-`VACUUM
    // INTO`). `VACUUM INTO` refuses to write over an existing destination,
    // and the previous verified backup at `backup_path` was never touched
    // by that interrupted run — this is purely tidying up, not a recovery
    // step.
    let _ = std::fs::remove_file(&temp_path);

    if let Some(required) = required_backup_space(db_path) {
        if let Some(available) = available_space(backup_path) {
            if available < required {
                tracing::warn!(
                    "db health routine: skipping backup — {available} byte(s) free, \
                     {required} required"
                );
                return DbHealthStatus {
                    last_run_at: Some(now),
                    outcome: Some(DbHealthOutcome::BackupSkippedInsufficientDiskSpace),
                    integrity_result: Some(integrity_result),
                    foreign_key_violations,
                    backup_path: existing_backup_path(backup_path),
                    ..Default::default()
                };
            }
        }
    }

    if let Err(e) = db.backup_into(&temp_path) {
        tracing::error!("db health routine: VACUUM INTO failed: {e}");
        let _ = std::fs::remove_file(&temp_path);
        return DbHealthStatus {
            last_run_at: Some(now),
            outcome: Some(DbHealthOutcome::BackupVerificationFailed),
            integrity_result: Some(integrity_result),
            foreign_key_violations,
            backup_path: existing_backup_path(backup_path),
            ..Default::default()
        };
    }

    finalize_backup(
        &temp_path,
        backup_path,
        integrity_result,
        foreign_key_violations,
        now,
    )
}

/// Verifies a freshly written backup at `temp_path` and, only if it passes
/// its own `integrity_check`, atomically replaces `backup_path` with it
/// (decision 5). On any failure the temp file is discarded and
/// `backup_path` — the previous verified backup, if any — is left exactly
/// as it was.
fn finalize_backup(
    temp_path: &Path,
    backup_path: &Path,
    integrity_result: String,
    foreign_key_violations: Vec<String>,
    now: DateTime<Utc>,
) -> DbHealthStatus {
    let verdict = integrity_check_file(temp_path);
    let verified_ok = matches!(&verdict, Ok(v) if v == "ok");

    if !verified_ok {
        match &verdict {
            Ok(v) => tracing::error!("db health routine: new backup failed integrity_check: {v}"),
            Err(e) => tracing::error!("db health routine: new backup verification errored: {e}"),
        }
        let _ = std::fs::remove_file(temp_path);
        return DbHealthStatus {
            last_run_at: Some(now),
            outcome: Some(DbHealthOutcome::BackupVerificationFailed),
            integrity_result: Some(integrity_result),
            foreign_key_violations,
            backup_path: existing_backup_path(backup_path),
            ..Default::default()
        };
    }

    if let Err(e) = std::fs::rename(temp_path, backup_path) {
        tracing::error!("db health routine: could not replace backup: {e}");
        let _ = std::fs::remove_file(temp_path);
        return DbHealthStatus {
            last_run_at: Some(now),
            outcome: Some(DbHealthOutcome::BackupVerificationFailed),
            integrity_result: Some(integrity_result),
            foreign_key_violations,
            backup_path: existing_backup_path(backup_path),
            ..Default::default()
        };
    }

    DbHealthStatus {
        last_run_at: Some(now),
        outcome: Some(DbHealthOutcome::Passed),
        integrity_result: Some(integrity_result),
        foreign_key_violations,
        backup_path: Some(backup_path.display().to_string()),
        ..Default::default()
    }
}

fn temp_backup_path(backup_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{TEMP_SUFFIX}", backup_path.display()))
}

fn existing_backup_path(backup_path: &Path) -> Option<String> {
    backup_path
        .exists()
        .then(|| backup_path.display().to_string())
}

/// Bytes of free space required to safely attempt a backup: the live
/// database's current file size plus a 10% margin. `None` when the file's
/// size can't be read — nothing to require, since there is then also
/// nothing to back up.
fn required_backup_space(db_path: &Path) -> Option<u64> {
    std::fs::metadata(db_path)
        .ok()
        .map(|m| m.len() * DISK_SPACE_MARGIN_NUMERATOR / DISK_SPACE_MARGIN_DENOMINATOR)
}

/// Free space on the filesystem holding `path`'s parent directory. `None`
/// when it can't be determined (e.g. no matching mount found) — callers
/// fail open rather than perpetually skipping backups on a platform where
/// this can't be read.
fn available_space(path: &Path) -> Option<u64> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(sysinfo::Disk::available_space)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::loops::{Loop, LoopStatus};
    use tempfile::tempdir;

    fn make_running_loop(id: &str) -> Loop {
        Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
            id: id.to_string(),
            name: format!("loop-{id}"),
            description: None,
            workdir: "/tmp/proj".to_string(),
            status: LoopStatus::Running,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        }
    }

    // ── run_health_check: pass path ───────────────────────────────────

    #[test]
    fn run_health_check_passes_on_a_healthy_database_and_writes_a_verified_backup() {
        let dir = tempdir().unwrap();
        let db_path = database_path(dir.path());
        let db = Database::new(&db_path).unwrap();
        db.insert_terminal_session("t1", "t1", "bash", "/tmp")
            .unwrap();

        let backup_path = backup_path(dir.path());
        let status = run_health_check(&db, &db_path, &backup_path);

        assert_eq!(status.outcome, Some(DbHealthOutcome::Passed));
        assert_eq!(status.integrity_result.as_deref(), Some("ok"));
        assert!(status.foreign_key_violations.is_empty());
        assert!(status.last_run_at.is_some());
        assert_eq!(status.backup_path, Some(backup_path.display().to_string()));
        assert!(backup_path.exists());
        assert_eq!(integrity_check_file(&backup_path).unwrap(), "ok");
        assert!(
            !temp_backup_path(&backup_path).exists(),
            "temp file must not survive a successful run"
        );
    }

    // ── run_health_check: integrity failure skips backup ──────────────

    #[test]
    fn run_health_check_skips_backup_when_integrity_check_fails() {
        let dir = tempdir().unwrap();
        let db_path = database_path(dir.path());
        {
            let db = Database::new(&db_path).unwrap();
            db.insert_terminal_session("t1", "t1", "bash", "/tmp")
                .unwrap();
            drop(db);
        }
        // Same corruption technique as `db::health`'s and `db::clean`'s own
        // tests: a valid header first, then bit-flipped page data, so this
        // is in-page corruption rather than a "not a database" error.
        let mut bytes = std::fs::read(&db_path).unwrap();
        let start = bytes.len() / 2;
        let end = start + 200.min(bytes.len() - start);
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }
        std::fs::write(&db_path, &bytes).unwrap();

        let db = Database::new(&db_path).unwrap();
        let backup_path = backup_path(dir.path());
        let status = run_health_check(&db, &db_path, &backup_path);

        assert_eq!(status.outcome, Some(DbHealthOutcome::IntegrityFailed));
        assert_ne!(status.integrity_result.unwrap(), "ok");
        assert!(
            !backup_path.exists(),
            "a corrupt source must never produce a backup"
        );
    }

    // ── backup failing its own verification (decision 5) ──────────────

    #[test]
    fn finalize_backup_leaves_previous_backup_untouched_when_new_backup_fails_verification() {
        let dir = tempdir().unwrap();
        let db_path = database_path(dir.path());
        let db = Database::new(&db_path).unwrap();
        db.insert_terminal_session("t1", "t1", "bash", "/tmp")
            .unwrap();

        let backup_path = backup_path(dir.path());
        // Seed a previous good backup.
        db.backup_into(&backup_path).unwrap();
        let previous_backup_bytes = std::fs::read(&backup_path).unwrap();

        // Produce a new backup for real, then deliberately corrupt it in
        // place — simulating damage that happened during (or just after)
        // the write, before verification runs.
        let temp_path = temp_backup_path(&backup_path);
        db.backup_into(&temp_path).unwrap();
        let mut bytes = std::fs::read(&temp_path).unwrap();
        let start = bytes.len() / 2;
        let end = start + 200.min(bytes.len() - start);
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }
        std::fs::write(&temp_path, &bytes).unwrap();

        let status = finalize_backup(
            &temp_path,
            &backup_path,
            "ok".to_string(),
            vec![],
            Utc::now(),
        );

        assert_eq!(
            status.outcome,
            Some(DbHealthOutcome::BackupVerificationFailed)
        );
        assert!(
            !temp_path.exists(),
            "the corrupted new backup must be discarded"
        );
        assert_eq!(
            std::fs::read(&backup_path).unwrap(),
            previous_backup_bytes,
            "the previous backup must be left byte-for-byte untouched"
        );
        assert_eq!(
            status.backup_path,
            Some(backup_path.display().to_string()),
            "status must still name the (old, still-valid) backup"
        );
    }

    // ── interrupted backup (crash mid-VACUUM INTO) ─────────────────────

    #[test]
    fn interrupted_backup_leaves_previous_backup_intact_and_the_next_run_recovers() {
        let dir = tempdir().unwrap();
        let db_path = database_path(dir.path());
        let db = Database::new(&db_path).unwrap();
        db.insert_terminal_session("t1", "t1", "bash", "/tmp")
            .unwrap();

        let backup_path = backup_path(dir.path());
        db.backup_into(&backup_path).unwrap();
        let previous_backup_bytes = std::fs::read(&backup_path).unwrap();

        // Simulate the daemon being killed mid-`VACUUM INTO` on a prior
        // run: an abandoned, partially-written temp file next to the
        // still-valid previous backup.
        let temp_path = temp_backup_path(&backup_path);
        std::fs::write(&temp_path, b"partial write, daemon died here").unwrap();

        // The interruption itself must never have touched the previous
        // backup.
        assert_eq!(std::fs::read(&backup_path).unwrap(), previous_backup_bytes);

        let status = run_health_check(&db, &db_path, &backup_path);

        assert_eq!(status.outcome, Some(DbHealthOutcome::Passed));
        assert!(
            !temp_path.exists(),
            "the abandoned temp file must be cleared before writing a new one"
        );
        assert_eq!(integrity_check_file(&backup_path).unwrap(), "ok");
    }

    // ── busy-skip recording ─────────────────────────────────────────────

    #[tokio::test]
    async fn maybe_run_records_a_skip_without_running_when_the_daemon_is_busy() {
        let dir = tempdir().unwrap();
        let db_path = database_path(dir.path());
        let db = Arc::new(Database::new(&db_path).unwrap());
        db.insert_loop(&make_running_loop("loop-1")).unwrap();

        let routine = HealthRoutine::new(Arc::clone(&db), dir.path().to_path_buf());
        routine.maybe_run().await;

        let status = load_status(&db);
        assert!(
            status.last_run_at.is_none(),
            "a busy daemon must not have actually run the routine"
        );
        assert!(status.last_skip_at.is_some());
        assert!(status.last_skip_reason.unwrap().contains("running"));
        assert!(!backup_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn maybe_run_runs_and_records_a_pass_when_idle() {
        let dir = tempdir().unwrap();
        let db_path = database_path(dir.path());
        let db = Arc::new(Database::new(&db_path).unwrap());

        let routine = HealthRoutine::new(Arc::clone(&db), dir.path().to_path_buf());
        routine.maybe_run().await;

        let status = load_status(&db);
        assert_eq!(status.outcome, Some(DbHealthOutcome::Passed));
        assert!(status.last_run_at.is_some());
        assert!(backup_path(dir.path()).exists());
    }

    // ── is_due gating inside maybe_run ──────────────────────────────────

    #[tokio::test]
    async fn maybe_run_does_nothing_when_not_yet_due() {
        let dir = tempdir().unwrap();
        let db_path = database_path(dir.path());
        let db = Arc::new(Database::new(&db_path).unwrap());
        save_status(
            &db,
            &DbHealthStatus {
                last_run_at: Some(Utc::now()),
                outcome: Some(DbHealthOutcome::Passed),
                ..Default::default()
            },
        )
        .unwrap();

        let routine = HealthRoutine::new(Arc::clone(&db), dir.path().to_path_buf());
        routine.maybe_run().await;

        assert!(
            !backup_path(dir.path()).exists(),
            "must not run again immediately after a fresh pass"
        );
    }
}
