use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::db::Database;

/// A one-shot scheduled prompt delivery.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ScheduledSend {
    pub id: String,
    pub prompt: String,
    pub target_session_id: String,
    /// Working directory of the target session at schedule time, so a
    /// dead-target failure can be preserved per-project (see
    /// `insert_failed_scheduled_send`). `None` if unknown.
    pub workdir: Option<String>,
    pub fire_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// A prompt whose scheduled delivery failed because its target session was
/// gone by fire time — preserved per-project so it can be recalled later
/// (see U8's last-prompt recall).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FailedScheduledSend {
    pub id: String,
    pub prompt: String,
    pub target_session_id: String,
    pub workdir: Option<String>,
    pub failed_at: DateTime<Utc>,
}

/// Whether `target_session_id` is among the currently live session ids.
/// Pure decision logic, kept separate from I/O so the fallback/dead-target
/// branch is unit-testable without a real PTY or database.
pub fn is_target_alive(target_session_id: &str, live_session_ids: &[String]) -> bool {
    live_session_ids.iter().any(|id| id == target_session_id)
}

impl Database {
    /// Insert a new scheduled send.
    pub fn insert_scheduled_send(
        &self,
        id: &str,
        prompt: &str,
        target_session_id: &str,
        workdir: Option<&str>,
        fire_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO scheduled_sends (id, prompt, target_session_id, workdir, fire_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, prompt, target_session_id, workdir, fire_at.timestamp(), now],
        )?;
        Ok(())
    }

    /// List all scheduled sends due at or before `now`, ordered by fire time.
    pub fn list_due_scheduled_sends(&self, now: DateTime<Utc>) -> Result<Vec<ScheduledSend>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, prompt, target_session_id, workdir, fire_at, created_at
             FROM scheduled_sends
             WHERE fire_at <= ?1
             ORDER BY fire_at ASC",
        )?;
        let rows = stmt.query_map(params![now.timestamp()], Self::row_to_scheduled_send)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Delete a scheduled send by ID. Returns true if a row was deleted.
    pub fn delete_scheduled_send(&self, id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute("DELETE FROM scheduled_sends WHERE id = ?1", params![id])?;
        Ok(rows > 0)
    }

    /// List all pending (not yet fired) scheduled sends for a given session,
    /// ordered by fire time (soonest first).
    pub fn list_pending_scheduled_sends_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<ScheduledSend>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, prompt, target_session_id, workdir, fire_at, created_at
             FROM scheduled_sends
             WHERE target_session_id = ?1
             ORDER BY fire_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id], Self::row_to_scheduled_send)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Re-point every scheduled send from `old_target` to `new_target`. Called
    /// when an interactive session is auto-resumed after a TUI restart: the
    /// resumed session gets a fresh runtime id, so pending schedules must be
    /// moved onto it or they would look orphaned and never fire. Returns the
    /// number of rows moved.
    pub fn reassign_scheduled_sends(&self, old_target: &str, new_target: &str) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE scheduled_sends SET target_session_id = ?2 WHERE target_session_id = ?1",
            params![old_target, new_target],
        )?;
        Ok(rows)
    }

    /// Silently delete every scheduled send whose target session is not in
    /// `live_targets`. Used on startup, after auto-resume, to drop schedules
    /// whose session no longer exists (never resumed). Returns rows deleted.
    /// An empty `live_targets` drops all pending scheduled sends.
    pub fn drop_scheduled_sends_missing_targets(&self, live_targets: &[String]) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        if live_targets.is_empty() {
            let rows = conn.execute("DELETE FROM scheduled_sends", [])?;
            return Ok(rows);
        }
        // Build a `(?,?,…)` placeholder list for the IN clause.
        let placeholders = vec!["?"; live_targets.len()].join(",");
        let sql =
            format!("DELETE FROM scheduled_sends WHERE target_session_id NOT IN ({placeholders})");
        let params = rusqlite::params_from_iter(live_targets.iter());
        let rows = conn.execute(&sql, params)?;
        Ok(rows)
    }

    fn row_to_scheduled_send(row: &rusqlite::Row) -> rusqlite::Result<ScheduledSend> {
        Ok(ScheduledSend {
            id: row.get(0)?,
            prompt: row.get(1)?,
            target_session_id: row.get(2)?,
            workdir: row.get(3)?,
            fire_at: DateTime::from_timestamp(row.get(4)?, 0)
                .ok_or_else(|| rusqlite::Error::InvalidParameterName("fire_at".into()))?,
            created_at: DateTime::from_timestamp(row.get(5)?, 0)
                .ok_or_else(|| rusqlite::Error::InvalidParameterName("created_at".into()))?,
        })
    }

    /// Preserve a prompt whose scheduled delivery failed because its target
    /// session no longer exists. Keeps the prompt recoverable per-project
    /// until U8's last-prompt recall (or a future consumer) picks it up.
    pub fn insert_failed_scheduled_send(
        &self,
        id: &str,
        prompt: &str,
        target_session_id: &str,
        workdir: Option<&str>,
        failed_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO failed_scheduled_sends (id, prompt, target_session_id, workdir, failed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                prompt,
                target_session_id,
                workdir,
                failed_at.timestamp()
            ],
        )?;
        Ok(())
    }

    /// List failed scheduled sends preserved for a given project workdir,
    /// most recent first. Not yet called from production code — the
    /// dead-target recovery path (see `data::deliver_due_scheduled_sends`)
    /// surfaces failures via `last_prompts` instead; this stays as the read
    /// side of `failed_scheduled_sends` for a future history browser.
    #[allow(dead_code)]
    pub fn list_failed_scheduled_sends_for_workdir(
        &self,
        workdir: &str,
    ) -> Result<Vec<FailedScheduledSend>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, prompt, target_session_id, workdir, failed_at
             FROM failed_scheduled_sends
             WHERE workdir = ?1
             ORDER BY failed_at DESC",
        )?;
        let rows = stmt.query_map(params![workdir], |row| {
            Ok(FailedScheduledSend {
                id: row.get(0)?,
                prompt: row.get(1)?,
                target_session_id: row.get(2)?,
                workdir: row.get(3)?,
                failed_at: DateTime::from_timestamp(row.get(4)?, 0)
                    .ok_or_else(|| rusqlite::Error::InvalidParameterName("failed_at".into()))?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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
    fn insert_and_list_due_scheduled_sends() {
        let db = test_db();
        let fire = Utc::now() - chrono::Duration::hours(1); // already due
        db.insert_scheduled_send("ss-1", "hello world", "session-abc", Some("/proj"), fire)
            .unwrap();

        let due = db.list_due_scheduled_sends(Utc::now()).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "ss-1");
        assert_eq!(due[0].prompt, "hello world");
        assert_eq!(due[0].target_session_id, "session-abc");
        assert_eq!(due[0].workdir.as_deref(), Some("/proj"));
    }

    #[test]
    fn list_due_excludes_future_sends() {
        let db = test_db();
        let future = Utc::now() + chrono::Duration::hours(1);
        db.insert_scheduled_send("ss-2", "later", "session-xyz", None, future)
            .unwrap();

        let due = db.list_due_scheduled_sends(Utc::now()).unwrap();
        assert!(due.is_empty());
    }

    #[test]
    fn delete_scheduled_send() {
        let db = test_db();
        let fire = Utc::now() - chrono::Duration::hours(1);
        db.insert_scheduled_send("ss-3", "to delete", "session-del", None, fire)
            .unwrap();

        assert!(db.delete_scheduled_send("ss-3").unwrap());
        assert!(!db.delete_scheduled_send("ss-3").unwrap());
        assert!(db.list_due_scheduled_sends(Utc::now()).unwrap().is_empty());
    }

    #[test]
    fn list_pending_for_session() {
        let db = test_db();
        let fire = Utc::now() + chrono::Duration::hours(2);
        db.insert_scheduled_send("ss-4", "for session", "session-42", None, fire)
            .unwrap();
        db.insert_scheduled_send("ss-5", "other session", "session-99", None, fire)
            .unwrap();

        let pending = db
            .list_pending_scheduled_sends_for_session("session-42")
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "ss-4");
    }

    /// Edit-in-place (B33): re-confirming an edited scheduled send REPLACES the
    /// existing row (delete old + insert edited) rather than adding a duplicate,
    /// so the pending count is unchanged and the content is updated.
    #[test]
    fn edit_in_place_replaces_without_duplicating() {
        let db = test_db();
        let fire = Utc::now() + chrono::Duration::hours(2);
        db.insert_scheduled_send("ss-edit", "original text", "session-1", Some("/proj"), fire)
            .unwrap();

        // The delete+insert path the edit flow uses.
        assert!(db.delete_scheduled_send("ss-edit").unwrap());
        db.insert_scheduled_send(
            "ss-edit-new",
            "edited text",
            "session-1",
            Some("/proj"),
            fire,
        )
        .unwrap();

        let pending = db
            .list_pending_scheduled_sends_for_session("session-1")
            .unwrap();
        assert_eq!(pending.len(), 1, "editing must not create a duplicate");
        assert_eq!(pending[0].prompt, "edited text");
        assert_eq!(pending[0].id, "ss-edit-new");
    }

    /// Cancelling a selected list entry (B33) removes exactly that one, leaving
    /// the surrounding entries — the list is ordered soonest-first, so index 1
    /// is the middle send.
    #[test]
    fn cancel_selected_removes_only_that_entry() {
        let db = test_db();
        let base = Utc::now() + chrono::Duration::hours(1);
        db.insert_scheduled_send("ss-a", "first", "session-x", None, base)
            .unwrap();
        db.insert_scheduled_send(
            "ss-b",
            "second",
            "session-x",
            None,
            base + chrono::Duration::hours(1),
        )
        .unwrap();
        db.insert_scheduled_send(
            "ss-c",
            "third",
            "session-x",
            None,
            base + chrono::Duration::hours(2),
        )
        .unwrap();

        let pending = db
            .list_pending_scheduled_sends_for_session("session-x")
            .unwrap();
        assert_eq!(pending[1].id, "ss-b");
        assert!(db.delete_scheduled_send(&pending[1].id).unwrap());

        let remaining = db
            .list_pending_scheduled_sends_for_session("session-x")
            .unwrap();
        let ids: Vec<&str> = remaining.iter().map(|send| send.id.as_str()).collect();
        assert_eq!(ids, vec!["ss-a", "ss-c"]);
    }

    #[test]
    fn reassign_moves_pending_sends_to_the_resumed_session_id() {
        let db = test_db();
        let fire = Utc::now() + chrono::Duration::hours(2);
        db.insert_scheduled_send("ss-r1", "keep me", "old-id", Some("/proj"), fire)
            .unwrap();
        db.insert_scheduled_send("ss-r2", "unrelated", "other-id", None, fire)
            .unwrap();

        let moved = db.reassign_scheduled_sends("old-id", "new-id").unwrap();
        assert_eq!(moved, 1);
        // The reassigned send now belongs to the resumed session id.
        assert!(db
            .list_pending_scheduled_sends_for_session("old-id")
            .unwrap()
            .is_empty());
        let pending = db
            .list_pending_scheduled_sends_for_session("new-id")
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "ss-r1");
        // The unrelated send is untouched.
        assert_eq!(
            db.list_pending_scheduled_sends_for_session("other-id")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn drop_missing_targets_removes_only_gone_sessions() {
        let db = test_db();
        let fire = Utc::now() + chrono::Duration::hours(1);
        db.insert_scheduled_send("ss-live", "deliver", "session-live", None, fire)
            .unwrap();
        db.insert_scheduled_send("ss-gone", "orphan", "session-gone", None, fire)
            .unwrap();

        let live = vec!["session-live".to_string()];
        let dropped = db.drop_scheduled_sends_missing_targets(&live).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(
            db.list_pending_scheduled_sends_for_session("session-live")
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .list_pending_scheduled_sends_for_session("session-gone")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn drop_missing_targets_with_no_live_sessions_clears_all() {
        let db = test_db();
        let fire = Utc::now() + chrono::Duration::hours(1);
        db.insert_scheduled_send("ss-x", "x", "s1", None, fire)
            .unwrap();
        db.insert_scheduled_send("ss-y", "y", "s2", None, fire)
            .unwrap();
        let dropped = db.drop_scheduled_sends_missing_targets(&[]).unwrap();
        assert_eq!(dropped, 2);
        assert!(db
            .list_due_scheduled_sends(Utc::now() + chrono::Duration::days(1))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn is_target_alive_true_when_session_present() {
        let live = vec!["session-a".to_string(), "session-b".to_string()];
        assert!(is_target_alive("session-b", &live));
    }

    #[test]
    fn is_target_alive_false_when_session_absent() {
        let live = vec!["session-a".to_string()];
        assert!(!is_target_alive("session-missing", &live));
        assert!(!is_target_alive("session-missing", &[]));
    }

    /// A due scheduled send is processed and removed — a second poll at the
    /// same (fake, injected) time must not find it again. Exercises the
    /// "fires once" requirement without depending on real wall-clock sleeps.
    #[test]
    fn scheduled_send_fires_once() {
        let db = test_db();
        let fake_now = Utc::now();
        let fire = fake_now - chrono::Duration::minutes(1);
        db.insert_scheduled_send("ss-once", "fire me", "session-live", None, fire)
            .unwrap();

        let due = db.list_due_scheduled_sends(fake_now).unwrap();
        assert_eq!(due.len(), 1);
        // Simulate successful delivery: remove after processing.
        assert!(db.delete_scheduled_send(&due[0].id).unwrap());

        // A later poll at the same fake "now" must not redeliver.
        let due_again = db.list_due_scheduled_sends(fake_now).unwrap();
        assert!(due_again.is_empty());
    }

    /// When a due send's target session is dead, the prompt must not be
    /// discarded silently — it is preserved in `failed_scheduled_sends`,
    /// keyed by the project workdir captured at schedule time.
    #[test]
    fn dead_target_preserves_prompt_for_recall() {
        let db = test_db();
        let fake_now = Utc::now();
        let fire = fake_now - chrono::Duration::minutes(1);
        db.insert_scheduled_send(
            "ss-dead",
            "please deliver me",
            "session-gone",
            Some("/home/user/project"),
            fire,
        )
        .unwrap();

        let due = db.list_due_scheduled_sends(fake_now).unwrap();
        assert_eq!(due.len(), 1);
        let send = &due[0];

        // No live sessions at all — the target is dead.
        assert!(!is_target_alive(&send.target_session_id, &[]));

        db.insert_failed_scheduled_send(
            &send.id,
            &send.prompt,
            &send.target_session_id,
            send.workdir.as_deref(),
            fake_now,
        )
        .unwrap();
        db.delete_scheduled_send(&send.id).unwrap();

        let preserved = db
            .list_failed_scheduled_sends_for_workdir("/home/user/project")
            .unwrap();
        assert_eq!(preserved.len(), 1);
        assert_eq!(preserved[0].prompt, "please deliver me");
        assert_eq!(preserved[0].target_session_id, "session-gone");

        // The scheduled send itself is gone — it will not be retried.
        assert!(db.list_due_scheduled_sends(fake_now).unwrap().is_empty());
    }
}
