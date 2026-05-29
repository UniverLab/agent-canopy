use anyhow::Result;
use chrono::Utc;
use rusqlite::params;

use crate::db::Database;

/// Record of an interactive agent session (persisted in SQLite).
#[allow(dead_code)]
pub struct InteractiveSession {
    pub id: String,
    pub name: String,
    pub cli: String,
    pub working_dir: String,
    pub args: Option<String>,
    pub started_at: String,
    pub status: String,
    pub session_type: String,
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
    pub fn insert_interactive_session(
        &self,
        id: &str,
        name: &str,
        cli: &str,
        working_dir: &str,
        args: Option<&str>,
        session_type: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO interactive_sessions (id, name, cli, working_dir, args, started_at, status, session_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
            params![id, name, cli, working_dir, args, Utc::now().to_rfc3339(), session_type],
        )?;
        Ok(())
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

    /// Get all sessions with status = 'active'.
    pub fn get_active_sessions(&self) -> Result<Vec<InteractiveSession>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cli, working_dir, args, started_at, status, session_type
             FROM interactive_sessions WHERE status = 'active' ORDER BY started_at DESC",
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
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark all 'active' sessions as 'orphaned' (called on startup cleanup).
    pub fn mark_orphaned_sessions(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE interactive_sessions SET status = 'orphaned' WHERE status = 'active'",
            [],
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
