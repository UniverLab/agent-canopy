use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};

use crate::application::ports::AgentRepository;
use crate::db::Database;
use crate::domain::models::{Agent, Cli, Trigger};

const AGENT_COLUMNS: &str = "id, prompt, trigger_type, trigger_config, cli, model, working_dir, \
                             enabled, created_at, log_path, timeout_minutes, expires_at, last_run_at, \
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

        conn.execute(
            &format!("INSERT OR REPLACE INTO agents ({AGENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"),
            params![
                &agent.id,
                &agent.prompt,
                trigger_type,
                trigger_config,
                agent.cli.as_str(),
                &agent.model,
                &agent.working_dir,
                agent.enabled,
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

        let row = stmt
            .query_row(params![id], |row| {
                Ok(AgentRow {
                    id: row.get(0)?,
                    prompt: row.get(1)?,
                    trigger_type: row.get(2)?,
                    trigger_config: row.get(3)?,
                    cli_str: row.get(4)?,
                    model: row.get(5)?,
                    working_dir: row.get(6)?,
                    enabled: row.get(7)?,
                    created_at_str: row.get(8)?,
                    log_path: row.get(9)?,
                    timeout_minutes: row.get(10)?,
                    expires_at_str: row.get(11)?,
                    last_run_at_str: row.get(12)?,
                    last_run_ok: row.get(13)?,
                    last_triggered_at_str: row.get(14)?,
                    trigger_count: row.get(15)?,
                })
            })
            .optional()?;

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

    fn delete_agent(&self, id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "DELETE FROM runs WHERE background_agent_id = ?1",
            params![id],
        )?;
        conn.execute("DELETE FROM agents WHERE id = ?1", params![id])?;
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
    fn list_agents_where(&self, where_clause: &str) -> Result<Vec<Agent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let sql =
            format!("SELECT {AGENT_COLUMNS} FROM agents {where_clause} ORDER BY created_at DESC");
        let mut stmt = conn.prepare(&sql)?;

        let rows = stmt.query_map([], |row| {
            Ok(AgentRow {
                id: row.get(0)?,
                prompt: row.get(1)?,
                trigger_type: row.get(2)?,
                trigger_config: row.get(3)?,
                cli_str: row.get(4)?,
                model: row.get(5)?,
                working_dir: row.get(6)?,
                enabled: row.get(7)?,
                created_at_str: row.get(8)?,
                log_path: row.get(9)?,
                timeout_minutes: row.get(10)?,
                expires_at_str: row.get(11)?,
                last_run_at_str: row.get(12)?,
                last_run_ok: row.get(13)?,
                last_triggered_at_str: row.get(14)?,
                trigger_count: row.get(15)?,
            })
        })?;

        let mut agents = Vec::new();
        for row_result in rows {
            agents.push(row_result?.into_agent()?);
        }
        Ok(agents)
    }
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
