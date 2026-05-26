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
