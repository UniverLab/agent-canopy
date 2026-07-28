//! Database operations for seed-session binding.
//!
//! The `seed_sessions` table maps interactive session IDs to their bound seed_id,
//! allowing the daemon to resolve identity from `identity.toml` at session start.

use anyhow::Result;
use rusqlite::params;

use crate::db::Database;

impl Database {
    /// Bind an interactive session to a seed identity.
    pub fn bind_session_to_seed(&self, session_id: &str, seed_id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO seed_sessions (session_id, seed_id, bound_at)
             VALUES (?1, ?2, ?3)",
            params![session_id, seed_id, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Resolve the seed_id for a given session_id.
    pub fn resolve_session_seed(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT seed_id FROM seed_sessions WHERE session_id = ?1")?;
        let result = stmt.query_row(params![session_id], |row| row.get(0)).ok();
        Ok(result)
    }

    /// Remove a seed-session binding (called when session ends).
    #[allow(dead_code)]
    pub fn unbind_session_seed(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "DELETE FROM seed_sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Get all active sessions bound to a specific seed.
    #[allow(dead_code)]
    pub fn get_sessions_for_seed(&self, seed_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT ss.session_id
             FROM seed_sessions ss
             JOIN interactive_sessions s ON s.id = ss.session_id
             WHERE ss.seed_id = ?1 AND s.status = 'active'
             ORDER BY ss.bound_at DESC",
        )?;
        let rows = stmt
            .query_map(params![seed_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    fn insert_test_session(db: &Database, session_id: &str) {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO interactive_sessions (id, name, cli, working_dir, started_at, status)
             VALUES (?1, 'test', 'bash', '/tmp', datetime('now'), 'active')",
            params![session_id],
        )
        .unwrap();
    }

    #[test]
    fn bind_and_resolve_session_seed() {
        let db = test_db();
        insert_test_session(&db, "session1");
        db.bind_session_to_seed("session1", "seed1").unwrap();

        let resolved = db.resolve_session_seed("session1").unwrap();
        assert_eq!(resolved, Some("seed1".to_string()));
    }

    #[test]
    fn resolve_session_seed_not_found() {
        let db = test_db();
        let resolved = db.resolve_session_seed("nonexistent").unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn unbind_session_seed() {
        let db = test_db();
        insert_test_session(&db, "session1");
        db.bind_session_to_seed("session1", "seed1").unwrap();

        db.unbind_session_seed("session1").unwrap();
        let resolved = db.resolve_session_seed("session1").unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn get_sessions_for_seed_empty() {
        let db = test_db();
        let sessions = db.get_sessions_for_seed("nonexistent").unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn bind_session_to_seed_overwrites() {
        let db = test_db();
        insert_test_session(&db, "session1");
        db.bind_session_to_seed("session1", "seed1").unwrap();
        db.bind_session_to_seed("session1", "seed2").unwrap();

        let resolved = db.resolve_session_seed("session1").unwrap();
        assert_eq!(resolved, Some("seed2".to_string()));
    }
}
