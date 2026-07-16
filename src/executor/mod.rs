//! Agent executor — spawns CLI subprocesses headlessly.
//!
//! Resolves the CLI binary path via `which`, spawns the process with
//! the appropriate flags, captures output to log files, and records
//! execution in the `runs` table.

use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;

use crate::application::notification_service::NotificationService;
use crate::application::ports::{AgentRepository, RunRepository};
use crate::db::Database;
use crate::domain::models::{Agent, Cli, RunLog, RunStatus, StartRunOutcome, Trigger, TriggerType};
use crate::scheduler::substitute_variables;

#[cfg(test)]
mod tests;

/// Maximum log file size before rotation (5 MB).
const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024;

/// Inputs for a single CLI execution.
struct CliRunParams<'a> {
    id: &'a str,
    cli: &'a Cli,
    prompt: String,
    model: Option<&'a str>,
    working_dir: Option<&'a str>,
    log_path: String,
    trigger: TriggerType,
}

/// Result of a CLI execution.
struct CliRunResult {
    exit_code: i32,
    success: bool,
}

/// Context for a single agent execution (file path + event for watch triggers).
struct ExecutionContext<'a> {
    file_path: Option<&'a str>,
    event_type: Option<&'a str>,
    trigger_type: TriggerType,
    /// True when this execution was triggered by a file-watch event.
    is_watch: bool,
    /// True when a human invoked this run directly (a forced `agent_run`),
    /// as opposed to the scheduler or a watch event firing it. Drives the
    /// success-notification policy (B27): manual runs always report success.
    is_manual: bool,
}

/// Agent execution engine.
pub struct Executor {
    db: Arc<Database>,
    notification_service: Arc<dyn NotificationService>,
}

impl Executor {
    pub fn new(db: Arc<Database>, notification_service: Arc<dyn NotificationService>) -> Self {
        Self {
            db,
            notification_service,
        }
    }

    /// Resolve a timed-out active run by marking it as timeout.
    fn resolve_timeout(&self, agent_id: &str) {
        let Ok(Some(run)) = self.db.get_active_run(agent_id) else {
            return;
        };
        let Some(timeout_at) = run.timeout_at else {
            return;
        };
        if Utc::now() <= timeout_at {
            return;
        }
        tracing::info!("Run '{}' for '{}' timed out, unlocking", run.id, agent_id);
        let _ = self
            .db
            .update_run_status(&run.id, RunStatus::Timeout, Some("Execution timed out"));
        let _ = self.db.update_agent_last_run(agent_id, false);
    }

    /// Atomically claim the right to run `agent`.
    ///
    /// This is the single choke point every firing path routes through —
    /// the scheduler's cron tick, a file-watch trigger, and a manual
    /// `agent_run` all end up here via [`Self::run_agent`] — so no two of
    /// them can ever spawn overlapping executions of the same agent, no
    /// matter which combination races. The check-then-insert itself is
    /// atomic (see [`crate::application::ports::RunRepository::try_start_run`]),
    /// closing the gap a bare "check active, then insert" would leave
    /// between the check and the write.
    ///
    /// Returns the new run's id if this call won the race, or `None` if
    /// another run was already active — in which case a `Missed` run is
    /// recorded (visible via `agent_logs`/recent executions, distinct from
    /// an agent that has never fired) and an INFO line is logged.
    fn start_run(&self, agent: &Agent, trigger_type: TriggerType) -> Result<Option<String>> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let timeout_at = now + chrono::Duration::minutes(i64::from(agent.timeout_minutes));
        let run = RunLog {
            id: run_id.clone(),
            background_agent_id: agent.id.clone(),
            status: RunStatus::Pending,
            trigger_type,
            summary: None,
            started_at: now,
            finished_at: None,
            exit_code: None,
            timeout_at: Some(timeout_at),
        };

        match self.db.try_start_run(&run)? {
            StartRunOutcome::Started => Ok(Some(run_id)),
            StartRunOutcome::AlreadyActive(active) => {
                tracing::info!(
                    "Agent '{}' is already running (run '{}'); skipping this fire",
                    agent.id,
                    active.id
                );
                let missed = RunLog {
                    id: uuid::Uuid::new_v4().to_string(),
                    background_agent_id: agent.id.clone(),
                    status: RunStatus::Missed,
                    trigger_type,
                    summary: Some(format!("Skipped: already running (run '{}')", active.id)),
                    started_at: now,
                    finished_at: Some(now),
                    exit_code: None,
                    timeout_at: None,
                };
                let _ = self.db.insert_run(&missed);
                Ok(None)
            }
        }
    }

    /// Finalize a run: update status, exit code, trigger count, and last_run.
    fn finalize_run(
        &self,
        agent: &Agent,
        run_id: &str,
        result: &CliRunResult,
        update_trigger_count: bool,
    ) {
        if let Ok(Some(run)) = self.db.get_run(run_id) {
            if run.status.is_active() {
                let status = if result.success {
                    RunStatus::Success
                } else {
                    RunStatus::Error
                };
                let _ = self.db.update_run_status(
                    run_id,
                    status,
                    Some(&format!(
                        "Auto-closed: process exited with code {}",
                        result.exit_code
                    )),
                );
            }
        }
        let _ = self.db.update_run_exit_code(run_id, result.exit_code);

        if let Err(e) = self.db.update_agent_last_run(&agent.id, result.success) {
            tracing::error!("Failed to update last_run for agent '{}': {}", agent.id, e);
        }

        if update_trigger_count {
            if let Err(e) = self.db.update_agent_triggered(&agent.id) {
                tracing::error!(
                    "Failed to update trigger count for agent '{}': {}",
                    agent.id,
                    e
                );
            }
        }
    }

    /// Send success/failure notification if the agent still exists.
    ///
    /// Failure-first policy (B27): a failed run always notifies. A *successful*
    /// run notifies only when a human ran it directly (`is_manual`) or the
    /// agent opted in via `notify_on_success` — otherwise a frequent cron/watch
    /// agent (e.g. a */15min schedule) would bury the Action Center under ~96
    /// success toasts a day.
    fn notify_result(&self, agent: &Agent, result: &CliRunResult, is_watch: bool, is_manual: bool) {
        let agent_still_exists = self.db.get_agent(&agent.id).ok().flatten().is_some();
        if !agent_still_exists {
            return;
        }
        if result.success {
            let opted_in = self.db.agent_notify_on_success(&agent.id).unwrap_or(false);
            if is_manual || opted_in {
                self.notification_service.notify_task_completed(
                    &agent.id,
                    true,
                    Some(result.exit_code),
                );
            }
        } else if is_watch {
            self.notification_service.notify_agent_failed(
                &agent.id,
                agent.cli.as_str(),
                result.exit_code,
                &format!("Watcher agent failed with exit code {}", result.exit_code),
            );
        } else {
            self.notification_service.notify_task_failed(
                &agent.id,
                result.exit_code,
                &format!("Agent failed with exit code {}", result.exit_code),
            );
        }
    }

    /// Core execution logic shared by all trigger types.
    async fn run_agent(&self, agent: &Agent, ctx: ExecutionContext<'_>) -> Result<i32> {
        self.resolve_timeout(&agent.id);

        let Some(run_id) = self.start_run(agent, ctx.trigger_type)? else {
            return Ok(-1);
        };

        let user_prompt = substitute_variables(
            &agent.prompt,
            &agent.id,
            &agent.log_path,
            ctx.file_path,
            ctx.event_type,
        );
        let wrapped = wrap_prompt(&user_prompt, &agent.id, &run_id);

        let params = CliRunParams {
            id: &agent.id,
            cli: &agent.cli,
            prompt: wrapped,
            model: agent.model.as_deref(),
            working_dir: agent.working_dir.as_deref(),
            log_path: agent.log_path.clone(),
            trigger: ctx.trigger_type,
        };

        let result = self.run_cli_process(&params).await?;

        let is_watch = ctx.is_watch || agent.is_watch();
        self.finalize_run(agent, &run_id, &result, is_watch);
        self.notify_result(agent, &result, ctx.is_watch, ctx.is_manual);

        Ok(result.exit_code)
    }

    /// Execute a unified agent.
    ///
    /// When `force` is true (manual runs), expiry and enabled checks are skipped.
    /// Returns the exit code if execution started, or -1 if skipped.
    pub async fn execute_agent(&self, agent: &Agent, force: bool) -> Result<i32> {
        let trigger_type = match &agent.trigger {
            Some(Trigger::Cron { .. }) => TriggerType::Scheduled,
            Some(Trigger::Watch { .. }) => TriggerType::Watch,
            None => TriggerType::Manual,
        };

        if !force {
            if agent.is_expired() {
                tracing::info!("Agent '{}' has expired, disabling", agent.id);
                self.db.update_agent_enabled(&agent.id, false)?;
                return Ok(-1);
            }
            if !agent.enabled {
                tracing::info!("Agent '{}' is disabled, skipping", agent.id);
                return Ok(-1);
            }
        }

        let ctx = ExecutionContext {
            file_path: if agent.is_watch() {
                Some("manual")
            } else {
                None
            },
            event_type: if agent.is_watch() {
                Some("manual")
            } else {
                None
            },
            trigger_type,
            is_watch: false,
            // A forced execution is a human running the agent on demand
            // (`agent_run`); the scheduler's cron tick passes `force = false`.
            is_manual: force,
        };

        self.run_agent(agent, ctx).await
    }

    /// Execute a watcher-triggered agent with specific file path and event info.
    pub async fn execute_agent_with_context(
        &self,
        agent: &Agent,
        file_path: &str,
        event_type: &str,
    ) -> Result<i32> {
        if !agent.enabled {
            return Ok(-1);
        }

        let ctx = ExecutionContext {
            file_path: Some(file_path),
            event_type: Some(event_type),
            trigger_type: TriggerType::Watch,
            is_watch: true,
            is_manual: false,
        };

        self.run_agent(agent, ctx).await
    }

    /// Core CLI execution: resolve binary, build command, spawn, capture output, write log.
    async fn run_cli_process(&self, params: &CliRunParams<'_>) -> Result<CliRunResult> {
        let cli_path = match resolve_cli_binary(params.cli) {
            Ok(path) => path,
            Err(e) => {
                tracing::error!("Failed to resolve CLI binary for '{}': {}", params.id, e);
                append_to_log(
                    &params.log_path,
                    params.id,
                    &params.trigger,
                    &Utc::now(),
                    -1,
                    &[],
                    e.to_string().as_bytes(),
                )?;
                return Ok(CliRunResult {
                    exit_code: -1,
                    success: false,
                });
            }
        };
        let mut cmd = match build_cli_command(
            &cli_path,
            params.cli,
            &params.prompt,
            params.model,
            params.working_dir,
        ) {
            Ok(cmd) => cmd,
            Err(e) => {
                tracing::error!("Failed to build command for '{}': {}", params.id, e);
                append_to_log(
                    &params.log_path,
                    params.id,
                    &params.trigger,
                    &Utc::now(),
                    -1,
                    &[],
                    e.to_string().as_bytes(),
                )?;
                return Ok(CliRunResult {
                    exit_code: -1,
                    success: false,
                });
            }
        };

        tracing::info!(
            "Executing '{}' with {} (trigger: {})",
            params.id,
            params.cli,
            params.trigger,
        );

        let started_at = Utc::now();
        let output = cmd.output().await;

        let (exit_code, success) = match output {
            Ok(out) => {
                let code = out.status.code().unwrap_or(-1);
                let success = out.status.success();
                append_to_log(
                    &params.log_path,
                    params.id,
                    &params.trigger,
                    &started_at,
                    code,
                    &out.stdout,
                    &out.stderr,
                )?;
                (code, success)
            }
            Err(e) => {
                tracing::error!("Failed to spawn CLI for '{}': {}", params.id, e);
                append_to_log(
                    &params.log_path,
                    params.id,
                    &params.trigger,
                    &started_at,
                    -1,
                    &[],
                    e.to_string().as_bytes(),
                )?;
                (-1, false)
            }
        };

        Ok(CliRunResult { exit_code, success })
    }
}

/// Resolve the full path to a CLI binary.
///
/// Delegates to `cli_strategy::resolve_binary`, which handles absolute
/// paths, PATH lookup, and the `~/.<binary>/bin/<binary>` fallback for
/// CLIs installed outside a PATH-minimal systemd user environment.
fn resolve_cli_binary(cli: &Cli) -> Result<PathBuf> {
    let cmd_name = cli.command_name();
    crate::domain::cli_strategy::resolve_binary(&cmd_name)
}

/// Build the CLI command with appropriate flags.
fn build_cli_command(
    _cli_path: &Path,
    cli: &Cli,
    prompt: &str,
    model: Option<&str>,
    working_dir: Option<&str>,
) -> Result<Command> {
    let strategy = cli.strategy();
    let mut cmd = strategy.build_command(prompt, model, working_dir)?;

    // Stdin is already set by `build_command` (null for argv-mode CLIs, or
    // an open temp-file handle carrying the prompt for stdin-mode CLIs) —
    // don't clobber it here.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    Ok(cmd)
}

/// Append execution output to an agent's log file with rotation.
fn append_to_log(
    log_path: &str,
    agent_id: &str,
    trigger: &TriggerType,
    started_at: &chrono::DateTime<Utc>,
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<()> {
    use std::io::Write;

    let path = Path::new(log_path);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    rotate_log_if_needed(path)?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    writeln!(file, "--- [{trigger}] {agent_id} at {started_at} ---")?;
    writeln!(file, "exit_code: {exit_code}")?;

    if !stdout.is_empty() {
        writeln!(file, "=== stdout ===")?;
        file.write_all(stdout)?;
        if !stdout.ends_with(b"\n") {
            writeln!(file)?;
        }
    }

    if !stderr.is_empty() {
        writeln!(file, "=== stderr ===")?;
        file.write_all(stderr)?;
        if !stderr.ends_with(b"\n") {
            writeln!(file)?;
        }
    }

    writeln!(file)?;
    Ok(())
}

/// Rotate log file if it exceeds `MAX_LOG_SIZE`.
fn rotate_log_if_needed(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.len() > MAX_LOG_SIZE {
            let rotated = path.with_extension("log.old");
            let _ = std::fs::remove_file(&rotated);
            std::fs::rename(path, &rotated)?;
            tracing::info!("Rotated log file: {}", path.display());
        }
    }
    Ok(())
}

/// Wrap the user's prompt with structured `agent_report` instructions.
fn wrap_prompt(user_prompt: &str, agent_id: &str, run_id: &str) -> String {
    format!(
        "[SYSTEM INSTRUCTIONS]\n\
         You are executing a managed agent. You MUST follow these steps:\n\
         1. IMMEDIATELY call the agent_report tool: agent_report(run_id=\"{run_id}\", status=\"in_progress\")\n\
         2. Execute the user's task below\n\
         3. When finished, call: agent_report(run_id=\"{run_id}\", status=\"success\", summary=\"<brief summary of what happened>\")\n\
            If the task failed: agent_report(run_id=\"{run_id}\", status=\"error\", summary=\"<what went wrong>\")\n\
         \n\
         Agent ID: {agent_id}\n\
         Run ID: {run_id}\n\
         [/SYSTEM INSTRUCTIONS]\n\
         \n\
         [USER TASK]\n\
         {user_prompt}\n\
         [/USER TASK]"
    )
}
