//! Repository functions backing `canopy clean` (soft cleanup, C1).
//!
//! Query/mutate helpers only — the decision of *what* is safe to remove
//! lives in `domain::clean` as pure functions over the facts these return.

use anyhow::Result;
use rusqlite::params;
use std::collections::HashSet;

use crate::db::Database;
use crate::domain::clean::{ProjectDependentCounts, SessionCandidate};

fn parse_rfc3339_ts(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

impl Database {
    /// `interactive_sessions` rows in the only statuses `canopy clean` (soft
    /// mode) is ever allowed to remove. `active` and `resumed` rows are
    /// excluded by this query itself, not just by the caller's later
    /// filtering, so a bug downstream can't widen the blast radius to a live
    /// or just-resumed session.
    pub fn list_cleanable_interactive_sessions(&self) -> Result<Vec<SessionCandidate>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, status, COALESCE(exited_at, started_at)
             FROM interactive_sessions
             WHERE status IN ('orphaned', 'error', 'completed')",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let status: String = row.get(1)?;
                let at: String = row.get(2)?;
                Ok((id, status, at))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(id, status, at)| SessionCandidate {
                id,
                status,
                age_ts: parse_rfc3339_ts(&at),
            })
            .collect())
    }

    /// Batch-delete `interactive_sessions` rows by id inside a single
    /// transaction, so a clean interrupted partway through can't leave the
    /// DB with only some of the planned rows gone.
    pub fn delete_interactive_sessions(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let tx = conn.transaction()?;
        let mut deleted = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM interactive_sessions WHERE id = ?1")?;
            for id in ids {
                deleted += stmt.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// All registered background-agent ids — cross-referenced against
    /// `logs/<id>.log` filenames to detect orphaned log files (an agent
    /// removed via `agent_remove` leaves its log file behind).
    pub fn list_agent_ids(&self) -> Result<HashSet<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT id FROM agents")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(rows)
    }

    /// All distinct `terminal_sessions.name` values — cross-referenced
    /// against `terminals/<name>/` directory names (terminal history is
    /// keyed by session *name*, not id) to detect orphaned history dirs.
    pub fn list_terminal_session_names(&self) -> Result<HashSet<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT DISTINCT name FROM terminal_sessions")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(rows)
    }

    /// Row counts that depend on a project's workdir, surfaced in the
    /// orphaned-project report.
    pub fn project_dependent_counts(&self, workdir: &str) -> Result<ProjectDependentCounts> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let loops: i64 = conn.query_row(
            "SELECT COUNT(*) FROM loops WHERE workdir = ?1",
            params![workdir],
            |row| row.get(0),
        )?;
        let interactive_sessions: i64 = conn.query_row(
            "SELECT COUNT(*) FROM interactive_sessions WHERE working_dir = ?1",
            params![workdir],
            |row| row.get(0),
        )?;
        let terminal_sessions: i64 = conn.query_row(
            "SELECT COUNT(*) FROM terminal_sessions WHERE working_dir = ?1",
            params![workdir],
            |row| row.get(0),
        )?;
        Ok(ProjectDependentCounts {
            loops,
            interactive_sessions,
            terminal_sessions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        // Leak the tempdir so the backing file survives for the DB's lifetime
        // within the test (mirrors the pattern used elsewhere in this crate).
        let path = dir.path().join("test.db");
        std::mem::forget(dir);
        Database::new(&path).unwrap()
    }

    #[test]
    fn list_cleanable_interactive_sessions_excludes_active_and_resumed() {
        let db = test_db();
        db.insert_interactive_session(
            "s-active",
            "active",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_interactive_session(
            "s-resumed",
            "resumed",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_interactive_session(
            "s-completed",
            "completed",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        // Force the statuses directly since insert always starts 'active'.
        db.mark_session_resumed("s-resumed").unwrap();
        db.finish_interactive_session("s-completed", 0).unwrap();

        let rows = db.list_cleanable_interactive_sessions().unwrap();
        let ids: HashSet<&str> = rows.iter().map(|s| s.id.as_str()).collect();
        assert!(!ids.contains("s-active"));
        assert!(!ids.contains("s-resumed"));
        assert!(ids.contains("s-completed"));
    }

    #[test]
    fn delete_interactive_sessions_is_transactional_batch() {
        let db = test_db();
        db.insert_interactive_session(
            "s1",
            "s1",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_interactive_session(
            "s2",
            "s2",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s1", 0).unwrap();
        db.finish_interactive_session("s2", 1).unwrap();

        let deleted = db
            .delete_interactive_sessions(&["s1".to_string(), "s2".to_string()])
            .unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(db.count_interactive_sessions().unwrap(), 0);
    }

    #[test]
    fn project_dependent_counts_reflect_workdir_scoped_rows() {
        let db = test_db();
        db.insert_interactive_session(
            "s1",
            "s1",
            "opencode",
            "/proj",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_terminal_session("t1", "t1", "bash", "/proj")
            .unwrap();

        let counts = db.project_dependent_counts("/proj").unwrap();
        assert_eq!(counts.interactive_sessions, 1);
        assert_eq!(counts.terminal_sessions, 1);
        assert_eq!(counts.loops, 0);

        let counts_other = db.project_dependent_counts("/elsewhere").unwrap();
        assert_eq!(counts_other.interactive_sessions, 0);
    }

    #[test]
    fn list_agent_ids_and_terminal_names_round_trip() {
        let db = test_db();
        assert!(db.list_agent_ids().unwrap().is_empty());
        db.insert_terminal_session("t1", "my-term", "bash", "/tmp")
            .unwrap();
        let names = db.list_terminal_session_names().unwrap();
        assert!(names.contains("my-term"));
    }
}
