use anyhow::Result;
use chrono::Utc;
use rusqlite::params;

use crate::db::Database;

/// Persist a split group to the database.
impl Database {
    pub fn insert_group(
        &self,
        id: &str,
        orientation: &str,
        session_a: &str,
        session_b: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO groups (id, orientation, session_a, session_b, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                orientation,
                session_a,
                session_b,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Remove a split group from the database.
    pub fn delete_group(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute("DELETE FROM groups WHERE id = ?1", params![id])?;
        Ok(())
    }
}
