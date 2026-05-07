use chrono::Utc;
use rmcp::model::CallToolResult;

use crate::application::ports::{AgentRepository, RunRepository};
use crate::daemon::helpers::{error_result, success_result};
use crate::daemon::params::*;
use crate::domain::models::{Agent, Cli, RunLog, RunStatus, Trigger, WatchEvent};
use crate::domain::validation::{validate_id, validate_prompt, validate_watch_path};

pub(crate) struct PreparedCronTask {
    pub cli: Cli,
    pub schedule_expr: String,
    pub expires_at: Option<chrono::DateTime<Utc>>,
}

pub(crate) struct PreparedWatchTask {
    pub cli: Cli,
    pub events: Vec<WatchEvent>,
    pub debounce_seconds: u64,
    pub recursive: bool,
}

pub(crate) fn prepare_cron_task(
    params: &TaskAddParams,
    validate_cron: &impl Fn(&str) -> bool,
) -> Result<PreparedCronTask, String> {
    validate_id(&params.id)?;
    validate_prompt(&params.prompt)?;

    Ok(PreparedCronTask {
        cli: Cli::resolve(params.cli.as_deref())?,
        schedule_expr: validate_cron_schedule(&params.schedule, validate_cron, |schedule| {
            format!(
                "Invalid cron expression '{}'. Must be a 5-field cron expression. \
                 Examples: '*/5 * * * *' (every 5 min), '0 9 * * *' (daily 9am).",
                schedule
            )
        })?,
        expires_at: params
            .duration_minutes
            .map(|minutes| Utc::now() + chrono::Duration::minutes(minutes)),
    })
}

pub(crate) fn prepare_watch_task(params: &TaskWatchParams) -> Result<PreparedWatchTask, String> {
    validate_id(&params.id)?;
    validate_prompt(&params.prompt)?;
    validate_watch_path(&params.path)?;

    Ok(PreparedWatchTask {
        cli: Cli::resolve(params.cli.as_deref())?,
        events: WatchEvent::parse_list(&params.events)?,
        debounce_seconds: params.debounce_seconds.unwrap_or(2),
        recursive: params.recursive.unwrap_or(false),
    })
}

pub(crate) fn apply_scalar_updates(
    agent: &mut Agent,
    params: &TaskUpdateParams,
) -> Result<(), String> {
    if let Some(prompt) = params.prompt.as_deref() {
        validate_prompt(prompt)?;
        agent.prompt = prompt.to_string();
    }
    if let Some(cli) = params.cli.as_deref() {
        agent.cli = Cli::from_str(cli);
    }
    if let Some(model) = params.model.as_ref() {
        agent.model = model.clone();
    }
    if let Some(working_dir) = params.working_dir.as_ref() {
        agent.working_dir = working_dir.clone();
    }
    if let Some(enabled) = params.enabled {
        agent.enabled = enabled;
    }
    Ok(())
}

pub(crate) fn apply_trigger_updates(
    agent: &mut Agent,
    params: &TaskUpdateParams,
    validate_cron: &impl Fn(&str) -> bool,
) -> Result<(), String> {
    match &mut agent.trigger {
        Some(Trigger::Cron { schedule_expr }) => {
            update_cron_trigger(schedule_expr, &mut agent.expires_at, params, validate_cron)
        }
        Some(Trigger::Watch {
            path,
            events,
            debounce_seconds,
            recursive,
        }) => update_watch_trigger(path, events, debounce_seconds, recursive, params),
        None => {
            create_trigger_from_update(params, validate_cron).map(|trigger| agent.trigger = trigger)
        }
    }
}

fn update_cron_trigger(
    schedule_expr: &mut String,
    expires_at: &mut Option<chrono::DateTime<Utc>>,
    params: &TaskUpdateParams,
    validate_cron: &impl Fn(&str) -> bool,
) -> Result<(), String> {
    if let Some(schedule) = params.schedule.as_deref() {
        *schedule_expr = validate_cron_schedule(schedule, validate_cron, |schedule| {
            format!("Invalid cron expression '{schedule}'.")
        })?;
    }
    if let Some(duration) = params.duration_minutes {
        *expires_at = update_expiration(duration)?;
    }
    Ok(())
}

fn update_watch_trigger(
    path: &mut String,
    events: &mut Vec<WatchEvent>,
    debounce_seconds: &mut u64,
    recursive: &mut bool,
    params: &TaskUpdateParams,
) -> Result<(), String> {
    if let Some(new_path) = params.path.as_deref() {
        validate_watch_path(new_path)?;
        *path = new_path.to_string();
    }
    if let Some(event_strs) = params.events.as_ref() {
        *events = WatchEvent::parse_list(event_strs)?;
    }
    if let Some(value) = params.debounce_seconds {
        *debounce_seconds = value;
    }
    if let Some(value) = params.recursive {
        *recursive = value;
    }
    Ok(())
}

fn create_trigger_from_update(
    params: &TaskUpdateParams,
    validate_cron: &impl Fn(&str) -> bool,
) -> Result<Option<Trigger>, String> {
    if let Some(schedule) = params.schedule.as_deref() {
        return validate_cron_schedule(schedule, validate_cron, |schedule| {
            format!("Invalid cron expression '{schedule}'.")
        })
        .map(|schedule_expr| Some(Trigger::Cron { schedule_expr }));
    }

    let Some(path) = params.path.as_deref() else {
        return Ok(None);
    };
    validate_watch_path(path)?;

    let events = match params.events.as_ref() {
        Some(event_strs) => WatchEvent::parse_list(event_strs)?,
        None => vec![WatchEvent::Create, WatchEvent::Modify],
    };

    Ok(Some(Trigger::Watch {
        path: path.to_string(),
        events,
        debounce_seconds: params.debounce_seconds.unwrap_or(2),
        recursive: params.recursive.unwrap_or(false),
    }))
}

pub(crate) fn watcher_restart_needed(params: &TaskUpdateParams) -> bool {
    params.path.is_some()
        || params.events.is_some()
        || params.debounce_seconds.is_some()
        || params.recursive.is_some()
        || params.cli.is_some()
        || params.prompt.is_some()
        || params.model.is_some()
}

fn validate_cron_schedule(
    schedule: &str,
    validate_cron: &impl Fn(&str) -> bool,
    invalid_message: impl FnOnce(&str) -> String,
) -> Result<String, String> {
    let trimmed = schedule.trim();
    if validate_cron(trimmed) {
        return Ok(trimmed.to_string());
    }
    Err(invalid_message(schedule))
}

fn update_expiration(duration: Option<i64>) -> Result<Option<chrono::DateTime<Utc>>, String> {
    match duration {
        Some(minutes) if minutes > 0 => Ok(Some(Utc::now() + chrono::Duration::minutes(minutes))),
        Some(_) => Err("duration_minutes must be positive".to_string()),
        None => Ok(None),
    }
}

pub(crate) fn parse_report_status(status: &str) -> Result<RunStatus, &'static str> {
    match status {
        "in_progress" => Ok(RunStatus::InProgress),
        "success" => Ok(RunStatus::Success),
        "error" => Ok(RunStatus::Error),
        _ => Err("Invalid status. Must be 'in_progress', 'success', or 'error'."),
    }
}

pub(crate) fn validate_report_summary(
    status: RunStatus,
    summary: Option<&str>,
) -> Result<(), &'static str> {
    if matches!(status, RunStatus::Success | RunStatus::Error) && summary.is_none() {
        return Err("A summary is required when reporting 'success' or 'error'.");
    }
    Ok(())
}

pub(crate) fn handle_timed_out_run(
    db: &crate::db::Database,
    run_id: &str,
    run: &RunLog,
) -> Option<CallToolResult> {
    let timeout_at = run.timeout_at?;
    if !run.status.is_active() || Utc::now() <= timeout_at {
        return None;
    }

    let _ = db.update_run_status(run_id, RunStatus::Timeout, Some("Execution timed out"));
    Some(error_result(&format!(
        "Run '{}' has timed out and can no longer be updated.",
        run_id
    )))
}

pub(crate) fn validate_run_transition(current: RunStatus, next: RunStatus) -> Result<(), String> {
    let valid = matches!(
        (current, next),
        (RunStatus::Pending, RunStatus::InProgress)
            | (RunStatus::InProgress, RunStatus::Success | RunStatus::Error)
            | (RunStatus::Pending, RunStatus::Success | RunStatus::Error)
    );
    if valid {
        return Ok(());
    }
    Err(format!("Invalid transition: {} -> {}", current, next))
}

pub(crate) fn update_agent_last_run(db: &crate::db::Database, run: &RunLog, status: RunStatus) {
    let success = match status {
        RunStatus::Success => Some(true),
        RunStatus::Error => Some(false),
        _ => None,
    };
    let Some(success) = success else {
        return;
    };

    let _ = db.update_agent_last_run(&run.background_agent_id, success);
}

pub(crate) fn new_agent_base(
    id: String,
    prompt: String,
    cli: Cli,
    model: Option<String>,
    working_dir: Option<String>,
    timeout_minutes: Option<u32>,
    log_path: String,
) -> Agent {
    Agent {
        id,
        prompt,
        cli,
        model,
        working_dir,
        enabled: true,
        created_at: Utc::now(),
        log_path,
        timeout_minutes: timeout_minutes.unwrap_or(15),
        expires_at: None,
        trigger: None,
        last_run_at: None,
        last_run_ok: None,
        last_triggered_at: None,
        trigger_count: 0,
    }
}

pub(crate) fn map_action_result<T>(
    result: Result<T, impl std::fmt::Display>,
    success_message: &str,
) -> CallToolResult {
    match result {
        Ok(_) => success_result(success_message),
        Err(e) => error_result(&e.to_string()),
    }
}
