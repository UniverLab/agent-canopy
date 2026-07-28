use anyhow::Result;
use chrono::Utc;
use rusqlite::params;

use crate::db::Database;

/// Record of an interactive agent session (persisted in SQLite).
#[allow(dead_code)]
#[derive(Clone)]
pub struct InteractiveSession {
    pub id: String,
    pub name: String,
    pub cli: String,
    pub working_dir: String,
    pub args: Option<String>,
    pub started_at: String,
    pub status: String,
    pub session_type: String,
    pub pid: Option<i64>,
    /// Machine boot id recorded when the session started (see
    /// `system::boot_id`). NULL for rows written before this column existed.
    pub boot_id: Option<String>,
}

#[allow(dead_code)]
pub struct TerminalSession {
    pub id: String,
    pub name: String,
    pub shell: String,
    pub working_dir: String,
    pub created_at: String,
}

impl Database {
    /// Insert a new interactive session as active.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_interactive_session(
        &self,
        id: &str,
        name: &str,
        cli: &str,
        working_dir: &str,
        args: Option<&str>,
        pid: Option<i64>,
        session_type: &str,
        boot_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO interactive_sessions (id, name, cli, working_dir, args, started_at, status, session_type, pid, boot_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9)",
            params![id, name, cli, working_dir, args, Utc::now().to_rfc3339(), session_type, pid, boot_id],
        )?;
        Ok(())
    }

    /// Get launch args for an interactive session by id.
    pub fn get_interactive_session_args(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT args FROM interactive_sessions WHERE id = ?1")?;
        let result = stmt.query_row(params![session_id], |row| row.get(0)).ok();
        Ok(result)
    }

    /// Get the status of an interactive session by id (`None` if no such row).
    /// Test-only inspection helper (B32) for asserting status transitions.
    #[cfg(test)]
    pub fn get_interactive_session_status(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT status FROM interactive_sessions WHERE id = ?1")?;
        let result = stmt.query_row(params![session_id], |row| row.get(0)).ok();
        Ok(result)
    }

    /// Get the working directory for an interactive session by id.
    pub fn get_session_workdir(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt =
            conn.prepare("SELECT working_dir FROM interactive_sessions WHERE id = ?1")?;
        let result = stmt.query_row(params![session_id], |row| row.get(0)).ok();
        Ok(result)
    }

    /// Mark a session as exited with a status and optional exit code.
    pub fn finish_interactive_session(&self, id: &str, exit_code: i32) -> Result<()> {
        let status = if exit_code == 0 { "completed" } else { "error" };
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE interactive_sessions SET exited_at = ?1, exit_code = ?2, status = ?3 WHERE id = ?4",
            params![Utc::now().to_rfc3339(), exit_code, status, id],
        )?;
        Ok(())
    }

    /// Get all sessions with status = 'active', excluding bridge sidecars.
    ///
    /// Bridge sessions (`canopy bridge`, see `daemon::bridge`) are proxy
    /// processes for an MCP harness, not resumable interactive CLIs — they
    /// must never be handed to `auto_resume_sessions`, which would try to
    /// relaunch `canopy bridge` as if it were a chat session.
    pub fn get_active_sessions(&self) -> Result<Vec<InteractiveSession>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cli, working_dir, args, started_at, status, session_type, pid, boot_id
             FROM interactive_sessions WHERE status = 'active' AND session_type != 'bridge'
             ORDER BY started_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(InteractiveSession {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    cli: row.get(2)?,
                    working_dir: row.get(3)?,
                    args: row.get(4)?,
                    started_at: row.get(5)?,
                    status: row.get(6)?,
                    session_type: row.get(7)?,
                    pid: row.get(8)?,
                    boot_id: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get all sessions with status = 'orphaned', excluding bridge sidecars.
    ///
    /// Populates the TUI's orphaned-sessions dialog, which lets the user
    /// manually revive or dismiss a session that couldn't be (or wasn't)
    /// auto-resumed at startup.
    pub fn get_orphaned_sessions(&self) -> Result<Vec<InteractiveSession>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cli, working_dir, args, started_at, status, session_type, pid, boot_id
             FROM interactive_sessions WHERE status = 'orphaned' AND session_type != 'bridge'
             ORDER BY started_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(InteractiveSession {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    cli: row.get(2)?,
                    working_dir: row.get(3)?,
                    args: row.get(4)?,
                    started_at: row.get(5)?,
                    status: row.get(6)?,
                    session_type: row.get(7)?,
                    pid: row.get(8)?,
                    boot_id: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get active sessions of a specific `session_type` (e.g. "bridge").
    ///
    /// Used at startup to reconcile bridge sidecars whose owning process
    /// died without calling `finish_standalone_session` — those rows would
    /// otherwise stay `active` forever.
    pub fn get_active_sessions_by_type(
        &self,
        session_type: &str,
    ) -> Result<Vec<InteractiveSession>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cli, working_dir, args, started_at, status, session_type, pid, boot_id
             FROM interactive_sessions WHERE status = 'active' AND session_type = ?1
             ORDER BY started_at DESC",
        )?;
        let rows = stmt
            .query_map(params![session_type], |row| {
                Ok(InteractiveSession {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    cli: row.get(2)?,
                    working_dir: row.get(3)?,
                    args: row.get(4)?,
                    started_at: row.get(5)?,
                    status: row.get(6)?,
                    session_type: row.get(7)?,
                    pid: row.get(8)?,
                    boot_id: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a single session 'orphaned'. Only transitions rows that are
    /// still 'active', so it's a no-op if the session was already resumed
    /// or handled elsewhere.
    ///
    /// Retained as a test-only fixture (B32): the product no longer orphans
    /// interactive sessions — an unrecoverable session is closed via
    /// [`Self::mark_session_closed`] instead — but tests still need a way to
    /// synthesize a historic `orphaned` row to exercise the startup sweep in
    /// [`Self::close_orphaned_interactive_sessions`].
    #[cfg(test)]
    pub fn mark_session_orphaned(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE interactive_sessions SET status = 'orphaned' WHERE id = ?1 AND status = 'active'",
            params![id],
        )?;
        Ok(())
    }

    /// Mark a single active session closed (`completed`) — the terminal state
    /// for a session auto-resume could not recover. There is no session-admin
    /// surface to revive an unrecoverable session, so rather than leaving it
    /// as a red, un-enterable `orphaned` row it simply becomes a finished
    /// session and disappears from the sidebar. The row is kept for history;
    /// only its status changes. Guarded to `active` rows like
    /// `mark_session_resumed`, so it's a no-op if the session was already
    /// resumed or finished elsewhere.
    pub fn mark_session_closed(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE interactive_sessions SET status = 'completed' WHERE id = ?1 AND status = 'active'",
            params![id],
        )?;
        Ok(())
    }

    /// Startup sweep: move any interactive session still in the retired
    /// `orphaned` status to `completed`, so historic red orphan rows written
    /// before orphaning was removed disappear from the sidebar. Returns the
    /// number of rows swept. Rows are kept for history — only the status
    /// changes.
    pub fn close_orphaned_interactive_sessions(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let rows = conn.execute(
            "UPDATE interactive_sessions SET status = 'completed' WHERE status = 'orphaned'",
            [],
        )?;
        Ok(rows)
    }

    /// Mark a single session 'resumed', once its replacement process has
    /// been launched. See `mark_session_orphaned` for why this is per-row
    /// rather than a mass update.
    pub fn mark_session_resumed(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE interactive_sessions SET status = 'resumed' WHERE id = ?1 AND status = 'active'",
            params![id],
        )?;
        Ok(())
    }

    /// Insert a terminal session record.
    pub fn insert_terminal_session(
        &self,
        id: &str,
        name: &str,
        shell: &str,
        working_dir: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO terminal_sessions (id, name, shell, working_dir, created_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'idle')",
            params![id, name, shell, working_dir, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Mark a terminal session as finished.
    pub fn finish_terminal_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE terminal_sessions SET status = 'finished', last_active = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    /// Update the working directory of an active terminal session.
    pub fn update_terminal_session_working_dir(&self, id: &str, working_dir: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE terminal_sessions SET working_dir = ?1, last_active = ?2 WHERE id = ?3",
            params![working_dir, Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    /// Get all terminal sessions that are still active (idle = was active when canopy last ran).
    pub fn get_active_terminal_sessions(&self) -> Result<Vec<TerminalSession>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, shell, working_dir, created_at
             FROM terminal_sessions WHERE status = 'idle' ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(TerminalSession {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    shell: row.get(2)?,
                    working_dir: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark all active terminal sessions as orphaned (called on startup cleanup).
    pub fn mark_orphaned_terminal_sessions(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE terminal_sessions SET status = 'orphaned' WHERE status = 'idle'",
            [],
        )?;
        Ok(())
    }

    /// Get the session_type for an interactive session by id.
    pub fn get_session_type(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt =
            conn.prepare("SELECT session_type FROM interactive_sessions WHERE id = ?1")?;
        let result = stmt.query_row(params![session_id], |row| row.get(0)).ok();
        Ok(result)
    }

    /// Remove an interactive session record entirely (used for nursery cleanup).
    pub fn remove_interactive_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "DELETE FROM interactive_sessions WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn count_interactive_sessions(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM interactive_sessions", [], |row| {
                row.get(0)
            })?;
        Ok(count)
    }

    pub fn count_terminal_sessions(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM terminal_sessions", [], |row| {
            row.get(0)
        })?;
        Ok(count)
    }

    pub fn count_background_agents(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn count_runs(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    #[test]
    fn insert_and_get_interactive_session() {
        let db = test_db();
        let session_id = "test-session-123";
        let name = "test-session";
        let cli = "bash";
        let workdir = "/tmp/test";
        let args = Some("arg1 arg2");
        let pid = Some(12345i64);
        let session_type = "interactive";
        let boot_id = Some("boot-123");

        db.insert_interactive_session(
            session_id,
            name,
            cli,
            workdir,
            args,
            pid,
            session_type,
            boot_id,
        )
        .unwrap();

        let retrieved_args = db.get_interactive_session_args(session_id).unwrap();
        assert!(retrieved_args.is_some());
        let retrieved_args = retrieved_args.unwrap();
        assert!(retrieved_args.contains("arg1"));
        assert!(retrieved_args.contains("arg2"));

        let workdir = db.get_session_workdir(session_id).unwrap();
        assert_eq!(workdir, Some("/tmp/test".to_string()));
    }

    #[test]
    fn get_interactive_session_args_not_found() {
        let db = test_db();
        let result = db.get_interactive_session_args("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn get_session_workdir_not_found() {
        let db = test_db();
        let result = db.get_session_workdir("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn finish_interactive_session() {
        let db = test_db();
        let session_id = "test-session-456";
        let name = "test-session";
        let cli = "bash";
        let workdir = "/tmp/test";
        let args = Some("arg1");
        let pid = Some(12345i64);
        let session_type = "interactive";
        let boot_id = Some("boot-123");

        db.insert_interactive_session(
            session_id,
            name,
            cli,
            workdir,
            args,
            pid,
            session_type,
            boot_id,
        )
        .unwrap();

        db.finish_interactive_session(session_id, 0).unwrap();

        let status = db.get_interactive_session_status(session_id).unwrap();
        assert_eq!(status, Some("completed".to_string()));
    }

    #[test]
    fn get_active_sessions_empty() {
        let db = test_db();
        let sessions = db.get_active_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn get_active_sessions_with_sessions() {
        let db = test_db();
        db.insert_interactive_session(
            "session1",
            "name1",
            "bash",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_interactive_session(
            "session2",
            "name2",
            "bash",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();

        let sessions = db.get_active_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn get_orphaned_sessions_empty() {
        let db = test_db();
        let sessions = db.get_orphaned_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn count_terminal_sessions_empty() {
        let db = test_db();
        let count = db.count_terminal_sessions().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn count_background_agents_empty() {
        let db = test_db();
        let count = db.count_background_agents().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn count_runs_empty() {
        let db = test_db();
        let count = db.count_runs().unwrap();
        assert_eq!(count, 0);
    }
}
