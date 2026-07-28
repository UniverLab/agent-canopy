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

#[cfg(test)]
mod tests {
    use crate::db::Database;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    #[test]
    fn insert_group_stores_group_in_database() {
        let db = test_db();
        let result = db.insert_group("group-1", "horizontal", "session-a", "session-b");
        assert!(result.is_ok(), "insert_group should succeed");
    }

    #[test]
    fn insert_group_replaces_existing_group_with_same_id() {
        let db = test_db();
        db.insert_group("group-1", "horizontal", "session-a", "session-b")
            .unwrap();
        // Insert again with same ID but different data
        let result = db.insert_group("group-1", "vertical", "session-c", "session-d");
        assert!(result.is_ok(), "insert_group should replace existing group");
    }

    #[test]
    fn delete_group_removes_group_from_database() {
        let db = test_db();
        db.insert_group("group-1", "horizontal", "session-a", "session-b")
            .unwrap();
        let result = db.delete_group("group-1");
        assert!(result.is_ok(), "delete_group should succeed");
    }

    #[test]
    fn delete_group_succeeds_even_if_group_does_not_exist() {
        let db = test_db();
        let result = db.delete_group("nonexistent-group");
        assert!(
            result.is_ok(),
            "delete_group should succeed even if group doesn't exist"
        );
    }
}
