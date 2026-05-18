//! SQLite repositories for collaborative sync messages and participant lookup.

use anyhow::Result;
use rusqlite::OptionalExtension;

use crate::db::Database;
use crate::domain::sync::{MessageKind, SyncMessage};

impl Database {
    pub fn insert_sync_message(
        &self,
        workdir: &str,
        agent_id: &str,
        agent_name: &str,
        kind: MessageKind,
        message: &str,
        payload: Option<&str>,
    ) -> Result<SyncMessage> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO sync_messages (workdir, agent_id, agent_name, kind, message, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![workdir, agent_id, agent_name, kind.as_str(), message, payload, now],
        )?;
        let id = conn.last_insert_rowid();

        Ok(SyncMessage {
            id,
            workdir: workdir.to_owned(),
            agent_id: agent_id.to_owned(),
            agent_name: agent_name.to_owned(),
            kind,
            message: message.to_owned(),
            payload: payload.map(str::to_owned),
            created_at: now,
        })
    }

    pub fn list_sync_messages(&self, workdir: &str, limit: usize) -> Result<Vec<SyncMessage>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, workdir, agent_id, agent_name, kind, message, payload, created_at
             FROM (
                SELECT id, workdir, agent_id, agent_name, kind, message, payload, created_at
                FROM sync_messages
                WHERE workdir = ?1
                ORDER BY created_at DESC, id DESC
                LIMIT ?2
             )
             ORDER BY created_at ASC, id ASC",
        )?;

        let rows = stmt.query_map(rusqlite::params![workdir, limit as i64], |row| {
            let kind_str: String = row.get(4)?;
            Ok(SyncMessage {
                id: row.get(0)?,
                workdir: row.get(1)?,
                agent_id: row.get(2)?,
                agent_name: row.get(3)?,
                kind: MessageKind::from_str(&kind_str).unwrap_or(MessageKind::Info),
                message: row.get(5)?,
                payload: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;

        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn list_recent_sync_messages(&self, limit: usize) -> Result<Vec<SyncMessage>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, workdir, agent_id, agent_name, kind, message, payload, created_at
             FROM (
                SELECT id, workdir, agent_id, agent_name, kind, message, payload, created_at
                FROM sync_messages
                ORDER BY created_at DESC, id DESC
                LIMIT ?1
             )
             ORDER BY created_at ASC, id ASC",
        )?;

        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            let kind_str: String = row.get(4)?;
            Ok(SyncMessage {
                id: row.get(0)?,
                workdir: row.get(1)?,
                agent_id: row.get(2)?,
                agent_name: row.get(3)?,
                kind: MessageKind::from_str(&kind_str).unwrap_or(MessageKind::Info),
                message: row.get(5)?,
                payload: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;

        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn resolve_sync_actor_name(&self, workdir: &str, agent_id: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        let interactive = conn
            .query_row(
                "SELECT name, cli
                 FROM interactive_sessions
                 WHERE id = ?1 AND working_dir = ?2
                 ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, started_at DESC
                 LIMIT 1",
                rusqlite::params![agent_id, workdir],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((name, cli)) = interactive {
            let display = if cli.is_empty() {
                name
            } else {
                format!("{name} · {cli}")
            };
            return Ok(Some(display));
        }

        let terminal_name = conn
            .query_row(
                "SELECT name
                 FROM terminal_sessions
                 WHERE id = ?1 AND working_dir = ?2
                 ORDER BY CASE status WHEN 'idle' THEN 0 ELSE 1 END, created_at DESC
                 LIMIT 1",
                rusqlite::params![agent_id, workdir],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        Ok(terminal_name)
    }

    pub fn resolve_sync_actor_display_name(&self, workdir: &str, agent_id: &str) -> Result<String> {
        Ok(self
            .resolve_sync_actor_name(workdir, agent_id)?
            .unwrap_or_else(|| agent_id.to_owned()))
    }

    /// Insert a closing marker into the sync channel when an agent exits.
    ///
    /// This allows `summarize_sync_context` to filter out stale missions from
    /// agents that have since left, without relying on the agent to self-report.
    pub fn close_agent_missions(
        &self,
        agent_id: &str,
        agent_name: &str,
        workdir: &str,
    ) -> Result<()> {
        self.insert_sync_message(
            workdir,
            agent_id,
            agent_name,
            MessageKind::Info,
            &format!("{agent_name} session ended — missions closed"),
            Some(r#"{"mission_closed":true}"#),
        )?;
        Ok(())
    }

    pub fn list_active_sync_agent_ids(&self, workdir: &str) -> Result<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id FROM interactive_sessions
             WHERE status = 'active' AND working_dir = ?1
             UNION
             SELECT id FROM terminal_sessions
             WHERE status = 'idle' AND working_dir = ?1
             UNION
             SELECT runs.background_agent_id
             FROM runs
             JOIN agents ON agents.id = runs.background_agent_id
             WHERE runs.status IN ('pending', 'in_progress')
               AND agents.working_dir = ?1",
        )?;

        let rows = stmt.query_map(rusqlite::params![workdir], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }
}
