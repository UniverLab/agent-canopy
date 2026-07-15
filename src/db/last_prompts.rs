use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::db::Database;

/// The most recent prompt sent (or scheduled, or recovered from a failed
/// scheduled delivery) from the prompt builder for a given project workdir.
/// Recalled into the builder with Ctrl+L (U8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastPrompt {
    pub id: String,
    pub workdir: String,
    pub prompt_text: String,
    /// JSON snapshot of the builder's structured fields (sections, tools,
    /// locked state, etc. — see `PersistedBuilderState`), when the write
    /// originated from an open builder. `None` when only the flattened
    /// prompt string survived (a scheduled send recovered after its
    /// delivery target died) — recall falls back to a single instruction
    /// section with the raw text in that case.
    pub builder_state: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Database {
    /// Record a prompt as the last one sent for `workdir`. Insert-only —
    /// see the `last_prompts` table comment for why reads take `LIMIT 1`
    /// instead of this being an upsert.
    pub fn insert_last_prompt(
        &self,
        id: &str,
        workdir: &str,
        prompt_text: &str,
        builder_state: Option<&str>,
        created_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO last_prompts (id, workdir, prompt_text, builder_state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                workdir,
                prompt_text,
                builder_state,
                created_at.timestamp()
            ],
        )?;
        Ok(())
    }

    /// The most recent prompt recorded for `workdir`, if any.
    pub fn get_last_prompt_for_workdir(&self, workdir: &str) -> Result<Option<LastPrompt>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, workdir, prompt_text, builder_state, created_at
             FROM last_prompts
             WHERE workdir = ?1
             ORDER BY created_at DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![workdir], |row| {
            Ok(LastPrompt {
                id: row.get(0)?,
                workdir: row.get(1)?,
                prompt_text: row.get(2)?,
                builder_state: row.get(3)?,
                created_at: DateTime::from_timestamp(row.get(4)?, 0)
                    .ok_or_else(|| rusqlite::Error::InvalidParameterName("created_at".into()))?,
            })
        })?;
        rows.next().transpose().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("test.db");
        Database::new(&db_path).unwrap()
    }

    #[test]
    fn round_trips_prompt_and_builder_state_per_workdir() {
        let db = test_db();
        db.insert_last_prompt(
            "lp-1",
            "/home/user/project-a",
            "do the thing",
            Some(r#"{"sections":{}}"#),
            Utc::now(),
        )
        .unwrap();

        let last = db
            .get_last_prompt_for_workdir("/home/user/project-a")
            .unwrap()
            .expect("prompt recorded");
        assert_eq!(last.prompt_text, "do the thing");
        assert_eq!(last.builder_state.as_deref(), Some(r#"{"sections":{}}"#));
    }

    #[test]
    fn two_projects_do_not_cross_contaminate() {
        let db = test_db();
        db.insert_last_prompt("lp-a", "/proj/a", "prompt for a", None, Utc::now())
            .unwrap();
        db.insert_last_prompt("lp-b", "/proj/b", "prompt for b", None, Utc::now())
            .unwrap();

        let last_a = db
            .get_last_prompt_for_workdir("/proj/a")
            .unwrap()
            .expect("a recorded");
        let last_b = db
            .get_last_prompt_for_workdir("/proj/b")
            .unwrap()
            .expect("b recorded");
        assert_eq!(last_a.prompt_text, "prompt for a");
        assert_eq!(last_b.prompt_text, "prompt for b");
    }

    #[test]
    fn unknown_workdir_returns_none() {
        let db = test_db();
        assert!(db
            .get_last_prompt_for_workdir("/never/sent")
            .unwrap()
            .is_none());
    }

    #[test]
    fn most_recent_write_wins_on_recall() {
        let db = test_db();
        let earlier = Utc::now() - chrono::Duration::minutes(5);
        let later = Utc::now();
        db.insert_last_prompt("lp-old", "/proj/a", "old prompt", None, earlier)
            .unwrap();
        db.insert_last_prompt("lp-new", "/proj/a", "new prompt", None, later)
            .unwrap();

        let last = db.get_last_prompt_for_workdir("/proj/a").unwrap().unwrap();
        assert_eq!(last.prompt_text, "new prompt");
        assert_eq!(last.id, "lp-new");
    }
}
