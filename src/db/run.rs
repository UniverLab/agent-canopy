use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};

use crate::application::ports::AgentRepository;
use crate::application::ports::RunRepository;
use crate::db::Database;
use crate::domain::models::{RunLog, RunStatus, TriggerType};

impl RunRepository for Database {
    fn insert_run(&self, run: &RunLog) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO runs (id, background_agent_id, status, trigger_type, summary, started_at, finished_at, exit_code, timeout_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &run.id,
                &run.background_agent_id,
                run.status.as_str(),
                run.trigger_type.as_str(),
                &run.summary,
                run.started_at.to_rfc3339(),
                run.finished_at.map(|t| t.to_rfc3339()),
                run.exit_code,
                run.timeout_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        drop(conn);
        self.upsert_run_intelligence_node(run)?;
        Ok(())
    }

    fn list_runs(&self, background_agent_id: &str, limit: usize) -> Result<Vec<RunLog>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, background_agent_id, status, trigger_type, summary, started_at, finished_at, exit_code, timeout_at
             FROM runs WHERE background_agent_id = ?1 ORDER BY started_at DESC LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![background_agent_id, limit as i64], |row| {
            Ok(RunRow {
                id: row.get(0)?,
                background_agent_id: row.get(1)?,
                status_str: row.get(2)?,
                trigger_str: row.get(3)?,
                summary: row.get(4)?,
                started_at_str: row.get(5)?,
                finished_at_str: row.get(6)?,
                exit_code: row.get(7)?,
                timeout_at_str: row.get(8)?,
            })
        })?;

        let mut runs = Vec::new();
        for row_result in rows {
            runs.push(row_result?.into_run_log()?);
        }
        Ok(runs)
    }

    fn list_all_recent_runs(&self, limit: usize) -> Result<Vec<RunLog>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, background_agent_id, status, trigger_type, summary, started_at, finished_at, exit_code, timeout_at
             FROM runs ORDER BY started_at DESC LIMIT ?1",
        )?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(RunRow {
                id: row.get(0)?,
                background_agent_id: row.get(1)?,
                status_str: row.get(2)?,
                trigger_str: row.get(3)?,
                summary: row.get(4)?,
                started_at_str: row.get(5)?,
                finished_at_str: row.get(6)?,
                exit_code: row.get(7)?,
                timeout_at_str: row.get(8)?,
            })
        })?;

        let mut runs = Vec::new();
        for row_result in rows {
            runs.push(row_result?.into_run_log()?);
        }
        Ok(runs)
    }

    fn get_active_run(&self, background_agent_id: &str) -> Result<Option<RunLog>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, background_agent_id, status, trigger_type, summary, started_at, finished_at, exit_code, timeout_at
             FROM runs WHERE background_agent_id = ?1 AND status IN ('pending', 'in_progress') LIMIT 1",
        )?;

        let run = stmt
            .query_row(params![background_agent_id], |row| {
                Ok(RunRow {
                    id: row.get(0)?,
                    background_agent_id: row.get(1)?,
                    status_str: row.get(2)?,
                    trigger_str: row.get(3)?,
                    summary: row.get(4)?,
                    started_at_str: row.get(5)?,
                    finished_at_str: row.get(6)?,
                    exit_code: row.get(7)?,
                    timeout_at_str: row.get(8)?,
                })
            })
            .optional()?;

        match run {
            Some(row) => Ok(Some(row.into_run_log()?)),
            None => Ok(None),
        }
    }

    fn update_run_status(
        &self,
        run_id: &str,
        status: RunStatus,
        summary: Option<&str>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let finished_at = if status.is_active() {
            None
        } else {
            Some(Utc::now().to_rfc3339())
        };
        let rows = conn.execute(
            "UPDATE runs SET status = ?1, summary = COALESCE(?2, summary), finished_at = COALESCE(?3, finished_at)
             WHERE id = ?4",
            params![status.as_str(), summary, finished_at, run_id],
        )?;
        if rows > 0 {
            drop(conn);
            if let Some(run) = self.get_run(run_id)? {
                self.upsert_run_intelligence_node(&run)?;
            }
        }
        Ok(rows > 0)
    }

    fn update_run_exit_code(&self, run_id: &str, exit_code: i32) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE runs SET exit_code = ?1 WHERE id = ?2",
            params![exit_code, run_id],
        )?;
        Ok(rows > 0)
    }

    fn get_run(&self, run_id: &str) -> Result<Option<RunLog>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, background_agent_id, status, trigger_type, summary, started_at, finished_at, exit_code, timeout_at
             FROM runs WHERE id = ?1",
        )?;

        let run = stmt
            .query_row(params![run_id], |row| {
                Ok(RunRow {
                    id: row.get(0)?,
                    background_agent_id: row.get(1)?,
                    status_str: row.get(2)?,
                    trigger_str: row.get(3)?,
                    summary: row.get(4)?,
                    started_at_str: row.get(5)?,
                    finished_at_str: row.get(6)?,
                    exit_code: row.get(7)?,
                    timeout_at_str: row.get(8)?,
                })
            })
            .optional()?;

        match run {
            Some(row) => Ok(Some(row.into_run_log()?)),
            None => Ok(None),
        }
    }
}

impl Database {
    fn upsert_run_intelligence_node(&self, run: &RunLog) -> Result<()> {
        let agent = self.get_agent(&run.background_agent_id)?;
        let working_dir = agent.as_ref().and_then(|item| item.working_dir.clone());
        let title = agent
            .as_ref()
            .map(|agent| {
                let first_line = agent.prompt.lines().next().unwrap_or(&agent.id);
                if first_line.is_empty() {
                    agent.id.as_str()
                } else {
                    first_line
                }
            })
            .unwrap_or(run.background_agent_id.as_str())
            .to_string();
        let body = match run.summary.as_deref() {
            Some(summary) => format!(
                "Run status: {}\nSummary: {}\nExit code: {}",
                run.status.as_str(),
                summary,
                run.exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "n/a".to_string())
            ),
            None => format!("Run status: {}", run.status.as_str()),
        };

        self.upsert_intelligence_node(crate::db::intelligence::IntelligenceNodeInput {
            id: Some(format!("run:{}", run.id)),
            kind: "session".to_string(),
            title,
            body,
            metadata: Some(serde_json::json!({
                "source": "run",
                "run_id": run.id,
                "background_agent_id": run.background_agent_id,
                "workdir": working_dir,
                "status": run.status.as_str(),
                "trigger_type": run.trigger_type.as_str(),
                "exit_code": run.exit_code,
                "started_at": run.started_at.to_rfc3339(),
                "finished_at": run.finished_at.map(|t| t.to_rfc3339()),
            })),
            project_hash: None,
            session_id: Some(run.id.clone()),
            relations: None,
        })?;

        Ok(())
    }
}

struct RunRow {
    id: String,
    background_agent_id: String,
    status_str: String,
    trigger_str: String,
    summary: Option<String>,
    started_at_str: String,
    finished_at_str: Option<String>,
    exit_code: Option<i32>,
    timeout_at_str: Option<String>,
}

impl RunRow {
    fn into_run_log(self) -> Result<RunLog> {
        let started_at =
            chrono::DateTime::parse_from_rfc3339(&self.started_at_str)?.with_timezone(&Utc);
        let finished_at = self
            .finished_at_str
            .as_ref()
            .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
            .transpose()?;
        let timeout_at = self
            .timeout_at_str
            .as_ref()
            .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
            .transpose()?;

        Ok(RunLog {
            id: self.id,
            background_agent_id: self.background_agent_id,
            status: RunStatus::from_str(&self.status_str),
            trigger_type: TriggerType::from_str(&self.trigger_str),
            summary: self.summary,
            started_at,
            finished_at,
            exit_code: self.exit_code,
            timeout_at,
        })
    }
}
