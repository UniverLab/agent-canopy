use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::domain::models::{Agent, CorruptAgent, RunLog, RunStatus};

// ── Repository traits ────────────────────────────────────────────────

/// Persistence operations for unified agents.
pub trait AgentRepository {
    fn upsert_agent(&self, agent: &Agent) -> Result<()>;
    fn get_agent(&self, id: &str) -> Result<Option<Agent>>;
    fn list_agents(&self) -> Result<Vec<Agent>>;
    fn list_cron_agents(&self) -> Result<Vec<Agent>>;
    fn list_watch_agents(&self) -> Result<Vec<Agent>>;
    /// Agents currently disabled with a pending one-shot `enable_at`.
    fn list_pending_enable_agents(&self) -> Result<Vec<Agent>>;
    /// Agent rows that failed to decode (e.g. malformed `trigger_config`
    /// written directly to SQLite by an external tool). Never errors the
    /// whole query and never attempts to repair or reinterpret the row.
    fn list_corrupt_agents(&self) -> Result<Vec<CorruptAgent>>;
    /// Deletes by id without parsing the stored row, so a corrupt row can
    /// always be removed. Returns whether a row was actually deleted.
    fn delete_agent(&self, id: &str) -> Result<bool>;
    fn rename_agent(&self, old_id: &str, new_id: &str, new_log_path: &str) -> Result<()>;
    fn update_agent_enabled(&self, id: &str, enabled: bool) -> Result<()>;
    /// Leave the agent disabled but set a one-shot `enable_at` time.
    fn schedule_agent_enable(&self, id: &str, at: DateTime<Utc>) -> Result<()>;
    /// Enable the agent and clear its `enable_at`, firing the one-shot schedule.
    fn activate_scheduled_enable(&self, id: &str) -> Result<()>;
    fn update_agent_last_run(&self, id: &str, success: bool) -> Result<()>;
    fn update_agent_triggered(&self, id: &str) -> Result<()>;
}

/// Persistence operations for execution run logs.
pub trait RunRepository {
    fn insert_run(&self, run: &RunLog) -> Result<()>;
    fn list_runs(&self, background_agent_id: &str, limit: usize) -> Result<Vec<RunLog>>;
    fn list_all_recent_runs(&self, limit: usize) -> Result<Vec<RunLog>>;
    fn get_active_run(&self, background_agent_id: &str) -> Result<Option<RunLog>>;
    fn update_run_status(
        &self,
        run_id: &str,
        status: RunStatus,
        summary: Option<&str>,
    ) -> Result<bool>;
    fn update_run_exit_code(&self, run_id: &str, exit_code: i32) -> Result<bool>;
    fn get_run(&self, run_id: &str) -> Result<Option<RunLog>>;
}

/// Key-value store for daemon state (e.g., PID, version).
pub trait StateRepository {
    fn set_state(&self, key: &str, value: &str) -> Result<()>;
    fn get_state(&self, key: &str) -> Result<Option<String>>;
}
