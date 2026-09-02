//! Repository functions backing the daemon's daily database health routine
//! (`daemon::health_routine`): the thorough sibling of [`super::clean`]'s
//! `quick_check` — full `PRAGMA integrity_check`, `PRAGMA foreign_key_check`,
//! and a verified `VACUUM INTO` backup. See `daemon::health_routine` for the
//! orchestration (idle gating, cadence, backup replacement) that calls these.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

use crate::db::Database;

impl Database {
    /// The thorough, single-pass-through-every-page integrity scan.
    /// Returns `"ok"` when the database is fine; any other string names the
    /// specific problem(s) `PRAGMA integrity_check` found (it can return
    /// multiple rows, joined here with `"; "`).
    ///
    /// Deliberately the full `integrity_check`, not [`super::clean::Database::quick_check`]:
    /// this routine runs on a daily timer with no service outage riding on
    /// it, so it can afford the thoroughness `clean`'s stop-the-daemon
    /// window and `doctor`'s interactive round-trip cannot.
    pub fn integrity_check(&self) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("PRAGMA integrity_check")?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.join("; "))
    }

    /// Rows `PRAGMA foreign_key_check` finds — orphaned rows that violate a
    /// declared foreign key. Empty when there are none. Each entry is one
    /// violation, formatted as `"<table> rowid=<rowid> -> <parent>"` (the
    /// `fkid` PRAGMA also returns is an internal index into the table's FK
    /// list, not something an operator can act on, so it's left out).
    ///
    /// Never fatal to this routine (decision 3): orphaned rows are a
    /// data-consistency issue that `canopy clean` resolves, not file
    /// corruption. This just surfaces them.
    pub fn foreign_key_check(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
        let rows = stmt
            .query_map([], |row| {
                let table: String = row.get(0)?;
                let rowid: Option<i64> = row.get(1)?;
                let parent: String = row.get(2)?;
                Ok(match rowid {
                    Some(id) => format!("{table} rowid={id} -> {parent}"),
                    None => format!("{table} -> {parent}"),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Writes a consistent, compacted copy of the live database to `dest`
    /// via `VACUUM INTO`, in one statement, without blocking writers.
    ///
    /// `dest` must not already exist — that's `VACUUM INTO`'s own
    /// requirement, and it's also why the caller always targets a fresh
    /// temp path rather than the final backup location directly (decision
    /// 4/5 in the health-routine spec: the previous backup is replaced only
    /// after the new one verifies, via rename).
    pub fn backup_into(&self, dest: &Path) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])?;
        Ok(())
    }
}

/// Runs `PRAGMA integrity_check` against an arbitrary database file, without
/// going through [`Database::new`] — which would run every migration and
/// seed builtin blueprints/prompts against it, mutating a file that's
/// supposed to be an inert, exact copy. Opened read-only so verifying a
/// backup can never itself write to it.
///
/// Used both to verify a freshly written backup before it replaces the
/// previous one, and (in tests) to assert a backup file's health directly.
pub fn integrity_check_file(path: &Path) -> Result<String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    // ── integrity_check ─────────────────────────────────────────────────

    #[test]
    fn integrity_check_reports_ok_for_a_healthy_database() {
        let db = test_db();
        assert_eq!(db.integrity_check().unwrap(), "ok");
    }

    #[test]
    fn integrity_check_reports_the_problem_for_a_corrupted_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.db");
        // Same technique as `db::clean`'s `quick_check` corruption test: a
        // real database first (valid header), then bit-flipped page data
        // after every handle is dropped, so this is in-page corruption
        // rather than a "not a database" file-format error.
        {
            let db = Database::new(&path).unwrap();
            db.insert_terminal_session("t1", "t1", "bash", "/tmp")
                .unwrap();
            drop(db);
        }
        let mut bytes = std::fs::read(&path).unwrap();
        // CM5: a fixed offset in page 3 rather than bytes.len()/2 — the
        // subagent_runs table shifted the file so the midpoint no longer
        // lands in a page quick_check validates.
        let start = 8192;
        let end = (start + 100).min(bytes.len());
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }
        std::fs::write(&path, &bytes).unwrap();

        let db = Database::new(&path).unwrap();
        let verdict = db.integrity_check().unwrap();
        assert_ne!(verdict, "ok", "corrupted file must not report ok");
    }

    // ── foreign_key_check ───────────────────────────────────────────────

    #[test]
    fn foreign_key_check_empty_for_a_healthy_database() {
        let db = test_db();
        assert!(db.foreign_key_check().unwrap().is_empty());
    }

    #[test]
    fn foreign_key_check_reports_an_orphaned_row() {
        let db = test_db();
        // Foreign keys are enforced on write (`PRAGMA foreign_keys=ON` in
        // `Database::new`), so an orphan can't be inserted through the
        // normal API — it has to be forced in directly with enforcement
        // off, the same way a real orphan could only arise from a bug or a
        // hand-edited file.
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "PRAGMA foreign_keys=OFF;
                 INSERT INTO loop_specs (id, loop_id, name, position, status)
                     VALUES ('orphan-spec', 'does-not-exist', 'orphan', 0, 'pending');
                 PRAGMA foreign_keys=ON;",
            )
            .unwrap();
        }
        let violations = db.foreign_key_check().unwrap();
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("loop_specs"));
    }

    // ── backup_into / integrity_check_file ─────────────────────────────

    #[test]
    fn backup_into_produces_a_file_that_passes_integrity_check() {
        let db = test_db();
        db.insert_terminal_session("t1", "t1", "bash", "/tmp")
            .unwrap();
        let dir = tempdir().unwrap();
        let dest = dir.path().join("backup.db");

        db.backup_into(&dest).unwrap();

        assert!(dest.exists());
        assert_eq!(integrity_check_file(&dest).unwrap(), "ok");
    }

    #[test]
    fn backup_into_fails_when_destination_already_exists() {
        let db = test_db();
        let dir = tempdir().unwrap();
        let dest = dir.path().join("backup.db");
        std::fs::write(&dest, b"already here").unwrap();

        assert!(db.backup_into(&dest).is_err());
    }

    #[test]
    fn integrity_check_file_reports_the_problem_for_a_corrupted_backup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt-backup.db");
        {
            let db = Database::new(&path).unwrap();
            db.insert_terminal_session("t1", "t1", "bash", "/tmp")
                .unwrap();
            drop(db);
        }
        let mut bytes = std::fs::read(&path).unwrap();
        // CM5: a fixed offset in page 3 rather than bytes.len()/2 — the
        // subagent_runs table shifted the file so the midpoint no longer
        // lands in a page quick_check validates.
        let start = 8192;
        let end = (start + 100).min(bytes.len());
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }
        std::fs::write(&path, &bytes).unwrap();

        let verdict = integrity_check_file(&path).unwrap();
        assert_ne!(verdict, "ok");
    }
}
