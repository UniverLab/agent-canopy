use anyhow::Result;

use crate::domain::models::{Agent, RunLog, RunStatus};

// ── Repository traits ────────────────────────────────────────────────

/// Persistence operations for unified agents.
pub trait AgentRepository {
    fn upsert_agent(&self, agent: &Agent) -> Result<()>;
    fn get_agent(&self, id: &str) -> Result<Option<Agent>>;
    fn list_agents(&self) -> Result<Vec<Agent>>;
    fn list_cron_agents(&self) -> Result<Vec<Agent>>;
    fn list_watch_agents(&self) -> Result<Vec<Agent>>;
    fn delete_agent(&self, id: &str) -> Result<()>;
    fn rename_agent(&self, old_id: &str, new_id: &str, new_log_path: &str) -> Result<()>;
    fn update_agent_enabled(&self, id: &str, enabled: bool) -> Result<()>;
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
