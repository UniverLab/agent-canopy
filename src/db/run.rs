use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};

use crate::application::ports::AgentRepository;
use crate::application::ports::RunRepository;
use crate::db::Database;
use crate::domain::models::{RunLog, RunStatus, StartRunOutcome, TriggerType};

impl RunRepository for Database {
    fn insert_run(&self, run: &RunLog) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        insert_run_row(&conn, run)?;
        drop(conn);
        self.upsert_run_intelligence_node(run)?;
        Ok(())
    }

    fn try_start_run(&self, run: &RunLog) -> Result<StartRunOutcome> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        // Check-then-insert under the *same* lock acquisition: two
        // concurrent callers (e.g. a scheduled tick and a manual
        // `agent_run`, or two evaluations of the same cron tick) serialize
        // on `self.conn`'s mutex, so whichever loses the race sees the
        // winner's row already inserted here instead of both seeing "no
        // active run" and both starting an execution.
        let active = {
            let mut stmt = conn.prepare(
                "SELECT id, background_agent_id, status, trigger_type, summary, started_at, finished_at, exit_code, timeout_at
                 FROM runs WHERE background_agent_id = ?1 AND status IN ('pending', 'in_progress') LIMIT 1",
            )?;
            stmt.query_row(params![&run.background_agent_id], |row| {
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
            .optional()?
        };

        if let Some(row) = active {
            return Ok(StartRunOutcome::AlreadyActive(row.into_run_log()?));
        }

        insert_run_row(&conn, run)?;
        drop(conn);
        self.upsert_run_intelligence_node(run)?;
        Ok(StartRunOutcome::Started)
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

/// Insert a run row on an already-locked connection. Shared by `insert_run`
/// and `try_start_run` so the INSERT itself stays single-sourced.
fn insert_run_row(conn: &rusqlite::Connection, run: &RunLog) -> Result<()> {
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
    Ok(())
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

#[cfg(test)]
mod tests {
    use crate::application::ports::RunRepository;
    use crate::db::Database;
    use crate::domain::models::{RunLog, RunStatus, StartRunOutcome, TriggerType};
    use chrono::Utc;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    fn make_run(agent_id: &str, status: RunStatus) -> RunLog {
        RunLog {
            id: format!("run-{}", agent_id),
            background_agent_id: agent_id.to_string(),
            status,
            trigger_type: TriggerType::Manual,
            summary: Some("test run".to_string()),
            started_at: Utc::now(),
            finished_at: None,
            exit_code: None,
            timeout_at: None,
        }
    }

    #[test]
    fn insert_run_stores_run_in_database() {
        let db = test_db();
        let run = make_run("agent-1", RunStatus::Pending);
        let result = db.insert_run(&run);
        assert!(result.is_ok(), "insert_run should succeed");

        let runs = db.list_runs("agent-1", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "run-agent-1");
    }

    #[test]
    fn list_runs_returns_runs_for_agent() {
        let db = test_db();
        let mut run1 = make_run("agent-1", RunStatus::Pending);
        run1.id = "run-1".to_string();
        let mut run2 = make_run("agent-1", RunStatus::InProgress);
        run2.id = "run-2".to_string();
        let mut run3 = make_run("agent-2", RunStatus::Pending);
        run3.id = "run-3".to_string();

        db.insert_run(&run1).unwrap();
        db.insert_run(&run2).unwrap();
        db.insert_run(&run3).unwrap();

        let runs = db.list_runs("agent-1", 10).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn list_runs_respects_limit() {
        let db = test_db();
        for i in 0..5 {
            let run = make_run(&format!("agent-{}", i), RunStatus::Pending);
            db.insert_run(&run).unwrap();
        }

        let runs = db.list_runs("agent-0", 3).unwrap();
        assert_eq!(runs.len(), 1); // Only one run for agent-0
    }

    #[test]
    fn try_start_run_returns_started_when_no_active_run() {
        let db = test_db();
        let run = make_run("agent-1", RunStatus::Pending);
        let outcome = db.try_start_run(&run).unwrap();
        assert!(matches!(outcome, StartRunOutcome::Started));
    }

    #[test]
    fn try_start_run_returns_already_active_when_run_exists() {
        let db = test_db();
        let run1 = make_run("agent-1", RunStatus::Pending);
        db.insert_run(&run1).unwrap();

        let run2 = make_run("agent-1", RunStatus::Pending);
        let outcome = db.try_start_run(&run2).unwrap();
        assert!(matches!(outcome, StartRunOutcome::AlreadyActive(_)));
    }
}
