use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::application::ports::StateRepository;
use crate::db::Database;

impl StateRepository for Database {
    fn set_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO daemon_state (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    fn get_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare("SELECT value FROM daemon_state WHERE key = ?1")?;
        let value = stmt.query_row(params![key], |row| row.get(0)).optional()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use crate::application::ports::StateRepository;
    use crate::db::Database;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    #[test]
    fn set_state_stores_value_in_database() {
        let db = test_db();
        let result = db.set_state("test-key", "test-value");
        assert!(result.is_ok(), "set_state should succeed");

        let value = db.get_state("test-key").unwrap();
        assert_eq!(value, Some("test-value".to_string()));
    }

    #[test]
    fn set_state_replaces_existing_value() {
        let db = test_db();
        db.set_state("test-key", "first-value").unwrap();
        db.set_state("test-key", "second-value").unwrap();

        let value = db.get_state("test-key").unwrap();
        assert_eq!(value, Some("second-value".to_string()));
    }

    #[test]
    fn get_state_returns_none_for_missing_key() {
        let db = test_db();
        let value = db.get_state("nonexistent-key").unwrap();
        assert_eq!(value, None);
    }

    #[test]
    fn get_state_returns_stored_value() {
        let db = test_db();
        db.set_state("my-key", "my-value").unwrap();

        let value = db.get_state("my-key").unwrap();
        assert_eq!(value, Some("my-value".to_string()));
    }
}
