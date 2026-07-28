use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};

use crate::application::ports::AgentRepository;
use crate::db::Database;
use crate::domain::models::{Agent, Cli, CorruptAgent, Trigger};

const AGENT_COLUMNS: &str = "id, prompt, trigger_type, trigger_config, cli, model, working_dir, \
                             enabled, enable_at, created_at, log_path, timeout_minutes, expires_at, last_run_at, \
                             last_run_ok, last_triggered_at, trigger_count";

impl AgentRepository for Database {
    fn upsert_agent(&self, agent: &Agent) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        let (trigger_type, trigger_config) = match &agent.trigger {
            Some(trigger) => (
                Some(trigger.type_str().to_string()),
                Some(serde_json::to_string(trigger)?),
            ),
            None => (None, None),
        };

        // Upsert via ON CONFLICT (not INSERT OR REPLACE): a replace would
        // delete the existing row and re-insert with column defaults, wiping
        // `notify_on_success` (B27) — which is deliberately absent from
        // `AGENT_COLUMNS` so it's owned solely by `set_agent_notify_on_success`
        // and survives every agent edit. Updating in place preserves it.
        let set_clause = AGENT_COLUMNS
            .split(", ")
            .filter(|col| *col != "id")
            .map(|col| format!("{col} = excluded.{col}"))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute(
            &format!(
                "INSERT INTO agents ({AGENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) \
                 ON CONFLICT(id) DO UPDATE SET {set_clause}"
            ),
            params![
                &agent.id,
                &agent.prompt,
                trigger_type,
                trigger_config,
                agent.cli.as_str(),
                &agent.model,
                &agent.working_dir,
                agent.enabled,
                agent.enable_at.map(|t| t.to_rfc3339()),
                agent.created_at.to_rfc3339(),
                &agent.log_path,
                agent.timeout_minutes as i64,
                agent.expires_at.map(|t| t.to_rfc3339()),
                agent.last_run_at.map(|t| t.to_rfc3339()),
                agent.last_run_ok,
                agent.last_triggered_at.map(|t| t.to_rfc3339()),
                agent.trigger_count as i64,
            ],
        )?;
        Ok(())
    }

    fn get_agent(&self, id: &str) -> Result<Option<Agent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt =
            conn.prepare(&format!("SELECT {AGENT_COLUMNS} FROM agents WHERE id = ?1"))?;

        let row = stmt.query_row(params![id], AgentRow::from_row).optional()?;

        match row {
            Some(r) => Ok(Some(r.into_agent()?)),
            None => Ok(None),
        }
    }

    fn list_agents(&self) -> Result<Vec<Agent>> {
        self.list_agents_where("")
    }

    fn list_cron_agents(&self) -> Result<Vec<Agent>> {
        self.list_agents_where("WHERE trigger_type = 'cron' AND enabled = 1")
    }

    fn list_watch_agents(&self) -> Result<Vec<Agent>> {
        self.list_agents_where("WHERE trigger_type = 'watch' AND enabled = 1")
    }

    fn list_pending_enable_agents(&self) -> Result<Vec<Agent>> {
        self.list_agents_where("WHERE enabled = 0 AND enable_at IS NOT NULL")
    }

    fn delete_agent(&self, id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "DELETE FROM runs WHERE background_agent_id = ?1",
            params![id],
        )?;
        let deleted = conn.execute("DELETE FROM agents WHERE id = ?1", params![id])?;
        Ok(deleted > 0)
    }

    fn rename_agent(&self, old_id: &str, new_id: &str, new_log_path: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        let tx = conn.unchecked_transaction()?;

        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1)",
            params![old_id],
            |row| row.get(0),
        )?;
        if !exists {
            anyhow::bail!("No agent found with ID '{old_id}'");
        }

        let taken: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1)",
            params![new_id],
            |row| row.get(0),
        )?;
        if taken {
            anyhow::bail!("An agent with ID '{new_id}' already exists");
        }

        tx.execute(
            "UPDATE agents SET id = ?1, log_path = ?2 WHERE id = ?3",
            params![new_id, new_log_path, old_id],
        )?;
        tx.execute(
            "UPDATE runs SET background_agent_id = ?1 WHERE background_agent_id = ?2",
            params![new_id, old_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    fn update_agent_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE agents SET enabled = ?1 WHERE id = ?2",
            params![enabled, id],
        )?;
        Ok(())
    }

    fn schedule_agent_enable(&self, id: &str, at: chrono::DateTime<Utc>) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE agents SET enabled = 0, enable_at = ?1 WHERE id = ?2",
            params![at.to_rfc3339(), id],
        )?;
        Ok(())
    }

    fn activate_scheduled_enable(&self, id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE agents SET enabled = 1, enable_at = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    fn update_agent_last_run(&self, id: &str, success: bool) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE agents SET last_run_at = ?1, last_run_ok = ?2 WHERE id = ?3",
            params![Utc::now().to_rfc3339(), success, id],
        )?;
        Ok(())
    }

    fn list_corrupt_agents(&self) -> Result<Vec<CorruptAgent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let sql = format!("SELECT {AGENT_COLUMNS} FROM agents ORDER BY created_at DESC");
        let mut stmt = conn.prepare(&sql)?;

        let rows = stmt.query_map([], AgentRow::from_row)?;

        let mut corrupt = Vec::new();
        for row_result in rows {
            if let Err(c) = decode_agent_row(row_result?) {
                corrupt.push(c);
            }
        }
        Ok(corrupt)
    }

    fn update_agent_triggered(&self, id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE agents SET last_triggered_at = ?1, trigger_count = trigger_count + 1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }
}

impl Database {
    /// Whether `id`'s agent has opted into success notifications (B27).
    /// Failures always notify regardless; this flag only governs whether a
    /// *successful* scheduled/watch run fires a toast. Stored outside
    /// `AGENT_COLUMNS` so `upsert_agent` never touches it; a missing agent
    /// reads as `false`.
    pub fn agent_notify_on_success(&self, id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let flag: Option<bool> = conn
            .query_row(
                "SELECT notify_on_success FROM agents WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(flag.unwrap_or(false))
    }

    /// Set (or clear) an agent's success-notification opt-in (B27).
    pub fn set_agent_notify_on_success(&self, id: &str, notify: bool) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE agents SET notify_on_success = ?1 WHERE id = ?2",
            params![notify, id],
        )?;
        Ok(())
    }

    /// Lists agents matching `where_clause`, silently skipping any row whose
    /// `trigger_config` (or other fields) fails to decode. Healthy agents are
    /// never affected; a corrupt row is simply absent here — callers that
    /// need visibility into corrupt rows use [`AgentRepository::list_corrupt_agents`].
    fn list_agents_where(&self, where_clause: &str) -> Result<Vec<Agent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let sql =
            format!("SELECT {AGENT_COLUMNS} FROM agents {where_clause} ORDER BY created_at DESC");
        let mut stmt = conn.prepare(&sql)?;

        let rows = stmt.query_map([], AgentRow::from_row)?;

        let mut agents = Vec::new();
        for row_result in rows {
            match decode_agent_row(row_result?) {
                Ok(agent) => agents.push(agent),
                Err(c) => tracing::debug!(
                    "Skipping corrupt agent row '{}' in list query: {}",
                    c.id,
                    c.error
                ),
            }
        }
        Ok(agents)
    }
}

/// The one lenient row-decoding path for agent rows: never reinterprets or
/// repairs malformed data, just reports it as [`CorruptAgent`] so callers can
/// quarantine or flag the row instead of taking down the whole query.
fn decode_agent_row(row: AgentRow) -> Result<Agent, CorruptAgent> {
    let id = row.id.clone();
    let enabled = row.enabled;
    row.into_agent().map_err(|e| CorruptAgent {
        id,
        enabled,
        error: e.to_string(),
    })
}

struct AgentRow {
    id: String,
    prompt: String,
    #[allow(dead_code)]
    trigger_type: Option<String>,
    trigger_config: Option<String>,
    cli_str: String,
    model: Option<String>,
    working_dir: Option<String>,
    enabled: bool,
    enable_at_str: Option<String>,
    created_at_str: String,
    log_path: String,
    timeout_minutes: i64,
    expires_at_str: Option<String>,
    last_run_at_str: Option<String>,
    last_run_ok: Option<bool>,
    last_triggered_at_str: Option<String>,
    trigger_count: i64,
}

impl AgentRow {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(AgentRow {
            id: row.get(0)?,
            prompt: row.get(1)?,
            trigger_type: row.get(2)?,
            trigger_config: row.get(3)?,
            cli_str: row.get(4)?,
            model: row.get(5)?,
            working_dir: row.get(6)?,
            enabled: row.get(7)?,
            enable_at_str: row.get(8)?,
            created_at_str: row.get(9)?,
            log_path: row.get(10)?,
            timeout_minutes: row.get(11)?,
            expires_at_str: row.get(12)?,
            last_run_at_str: row.get(13)?,
            last_run_ok: row.get(14)?,
            last_triggered_at_str: row.get(15)?,
            trigger_count: row.get(16)?,
        })
    }

    fn into_agent(self) -> Result<Agent> {
        let cli = Cli::from_str(&self.cli_str);
        let created_at =
            chrono::DateTime::parse_from_rfc3339(&self.created_at_str)?.with_timezone(&Utc);
        let expires_at = self
            .expires_at_str
            .as_ref()
            .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
            .transpose()?;
        let last_run_at = self
            .last_run_at_str
            .as_ref()
            .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
            .transpose()?;
        let last_triggered_at = self
            .last_triggered_at_str
            .as_ref()
            .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
            .transpose()?;
        let enable_at = self
            .enable_at_str
            .as_ref()
            .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
            .transpose()?;

        let trigger = self
            .trigger_config
            .as_ref()
            .map(|json| serde_json::from_str::<Trigger>(json))
            .transpose()?;

        Ok(Agent {
            id: self.id,
            prompt: self.prompt,
            trigger,
            cli,
            model: self.model,
            working_dir: self.working_dir,
            enabled: self.enabled,
            enable_at,
            created_at,
            log_path: self.log_path,
            timeout_minutes: self.timeout_minutes as u32,
            expires_at,
            last_run_at,
            last_run_ok: self.last_run_ok,
            last_triggered_at,
            trigger_count: self.trigger_count as u64,
        })
    }
}

#[cfg(test)]
impl Database {
    /// Inserts an agent row with an unparseable `trigger_config` directly via
    /// SQL, bypassing `upsert_agent`'s JSON serialization. Mirrors the real
    /// incident this exists to guard against: an external tool writing a raw
    /// cron string (`11 3 11 7 *`) into a column where canopy expects JSON
    /// (`{"type":"cron","schedule_expr":"..."}`).
    pub(crate) fn insert_corrupt_agent_for_test(&self, id: &str, enabled: bool) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO agents (id, prompt, trigger_type, trigger_config, cli, enabled, created_at, log_path, timeout_minutes, trigger_count)
             VALUES (?1, 'corrupt test agent', 'cron', ?2, 'opencode', ?3, ?4, ?5, 15, 0)",
            params![
                id,
                "11 3 11 7 *",
                enabled,
                Utc::now().to_rfc3339(),
                format!("/tmp/{id}.log"),
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{Agent, Cli, Trigger, WatchEvent};
    use chrono::Utc;

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    fn sample_agent(id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "test prompt".to_string(),
            trigger: Some(Trigger::Cron {
                schedule_expr: "0 9 * * *".to_string(),
            }),
            cli: Cli::new("opencode"),
            model: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: Utc::now(),
            log_path: format!("/tmp/{id}.log"),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    #[test]
    fn upsert_and_get_agent() {
        let db = test_db();
        let agent = sample_agent("test-agent");
        db.upsert_agent(&agent).unwrap();

        let retrieved = db.get_agent("test-agent").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "test-agent");
        assert_eq!(retrieved.prompt, "test prompt");
    }

    #[test]
    fn get_agent_not_found() {
        let db = test_db();
        let result = db.get_agent("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_agents_empty() {
        let db = test_db();
        let agents = db.list_agents().unwrap();
        assert!(agents.is_empty());
    }

    #[test]
    fn list_agents_with_agents() {
        let db = test_db();
        let agent1 = sample_agent("agent1");
        let agent2 = sample_agent("agent2");
        db.upsert_agent(&agent1).unwrap();
        db.upsert_agent(&agent2).unwrap();

        let agents = db.list_agents().unwrap();
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn list_cron_agents() {
        let db = test_db();
        let cron_agent = sample_agent("cron1");
        let mut watch_agent = sample_agent("watch1");
        watch_agent.trigger = Some(Trigger::Watch {
            path: "/tmp".to_string(),
            events: vec![WatchEvent::Create],
            debounce_seconds: 2,
            recursive: false,
        });

        db.upsert_agent(&cron_agent).unwrap();
        db.upsert_agent(&watch_agent).unwrap();

        let cron_agents = db.list_cron_agents().unwrap();
        assert_eq!(cron_agents.len(), 1);
        assert_eq!(cron_agents[0].id, "cron1");
    }

    #[test]
    fn list_watch_agents() {
        let db = test_db();
        let mut cron_agent = sample_agent("cron1");
        cron_agent.trigger = Some(Trigger::Cron {
            schedule_expr: "0 9 * * *".to_string(),
        });
        let mut watch_agent = sample_agent("watch1");
        watch_agent.trigger = Some(Trigger::Watch {
            path: "/tmp".to_string(),
            events: vec![WatchEvent::Create],
            debounce_seconds: 2,
            recursive: false,
        });

        db.upsert_agent(&cron_agent).unwrap();
        db.upsert_agent(&watch_agent).unwrap();

        let watch_agents = db.list_watch_agents().unwrap();
        assert_eq!(watch_agents.len(), 1);
        assert_eq!(watch_agents[0].id, "watch1");
    }

    #[test]
    fn delete_agent() {
        let db = test_db();
        let agent = sample_agent("test-agent");
        db.upsert_agent(&agent).unwrap();

        let deleted = db.delete_agent("test-agent").unwrap();
        assert!(deleted);

        let retrieved = db.get_agent("test-agent").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn delete_agent_not_found() {
        let db = test_db();
        let deleted = db.delete_agent("nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn update_agent_enabled() {
        let db = test_db();
        let agent = sample_agent("test-agent");
        db.upsert_agent(&agent).unwrap();

        db.update_agent_enabled("test-agent", false).unwrap();
        let retrieved = db.get_agent("test-agent").unwrap().unwrap();
        assert!(!retrieved.enabled);

        db.update_agent_enabled("test-agent", true).unwrap();
        let retrieved = db.get_agent("test-agent").unwrap().unwrap();
        assert!(retrieved.enabled);
    }

    #[test]
    fn update_agent_last_run() {
        let db = test_db();
        let agent = sample_agent("test-agent");
        db.upsert_agent(&agent).unwrap();

        db.update_agent_last_run("test-agent", true).unwrap();
        let retrieved = db.get_agent("test-agent").unwrap().unwrap();
        assert!(retrieved.last_run_at.is_some());
        assert_eq!(retrieved.last_run_ok, Some(true));
    }

    #[test]
    fn update_agent_triggered() {
        let db = test_db();
        let agent = sample_agent("test-agent");
        db.upsert_agent(&agent).unwrap();

        db.update_agent_triggered("test-agent").unwrap();
        let retrieved = db.get_agent("test-agent").unwrap().unwrap();
        assert_eq!(retrieved.trigger_count, 1);
        assert!(retrieved.last_triggered_at.is_some());
    }

    #[test]
    fn list_corrupt_agents_empty() {
        let db = test_db();
        let corrupt = db.list_corrupt_agents().unwrap();
        assert!(corrupt.is_empty());
    }

    #[test]
    fn rename_agent() {
        let db = test_db();
        let agent = sample_agent("old-id");
        db.upsert_agent(&agent).unwrap();

        db.rename_agent("old-id", "new-id", "/tmp/new-id.log")
            .unwrap();

        let old = db.get_agent("old-id").unwrap();
        assert!(old.is_none());

        let new = db.get_agent("new-id").unwrap().unwrap();
        assert_eq!(new.id, "new-id");
        assert_eq!(new.log_path, "/tmp/new-id.log");
    }
}
