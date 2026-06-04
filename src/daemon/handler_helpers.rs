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

// ── Intelligence context helpers ─────────────────────────────────

use crate::db::Database;
use crate::shared::sync_identity::{self, header_str};

/// Try to auto-detect the effective project_hash from session workdir,
/// falling back to the caller-supplied value.
pub(crate) fn resolve_effective_project_hash(
    db: &Database,
    provided: Option<&str>,
    agent_id: &str,
) -> Option<String> {
    if let Some(ph) = provided {
        return Some(ph.to_string());
    }
    db.get_session_workdir(agent_id)
        .ok()
        .flatten()
        .map(|wd| crate::domain::project::workdir_hash(&wd))
}

/// Load the seed identity bound to the current session agent.
/// Returns (seed_id, identity) after resolving from seed header or DB mapping.
pub(crate) fn load_bound_seed_identity(
    db: &crate::db::Database,
    parts: &axum::http::request::Parts,
    agent_id: &str,
) -> Result<(String, crate::domain::seeds::SeedIdentity), String> {
    let seed_id = if let Some(sid) = header_str(parts, sync_identity::CANOPY_SEED_ID_HEADER) {
        let _ = db.bind_session_to_seed(agent_id, sid);
        sid.to_string()
    } else {
        db.resolve_session_seed(agent_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "No seed identity bound to this session.".to_string())?
    };

    let identity = crate::domain::seeds::load_seed(&seed_id)?;
    Ok((seed_id, identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::RunStatus;

    #[test]
    fn parse_report_status_valid() {
        assert!(matches!(
            parse_report_status("in_progress"),
            Ok(RunStatus::InProgress)
        ));
        assert!(matches!(
            parse_report_status("success"),
            Ok(RunStatus::Success)
        ));
        assert!(matches!(parse_report_status("error"), Ok(RunStatus::Error)));
    }

    #[test]
    fn parse_report_status_invalid() {
        assert!(parse_report_status("invalid").is_err());
        assert!(parse_report_status("").is_err());
        assert!(parse_report_status("pending").is_err());
    }

    #[test]
    fn validate_report_summary_success_requires_summary() {
        assert!(validate_report_summary(RunStatus::Success, Some("done")).is_ok());
        assert!(validate_report_summary(RunStatus::Success, None).is_err());
    }

    #[test]
    fn validate_report_summary_error_requires_summary() {
        assert!(validate_report_summary(RunStatus::Error, Some("failed")).is_ok());
        assert!(validate_report_summary(RunStatus::Error, None).is_err());
    }

    #[test]
    fn validate_report_summary_in_progress_no_summary_needed() {
        assert!(validate_report_summary(RunStatus::InProgress, None).is_ok());
        assert!(validate_report_summary(RunStatus::InProgress, Some("working")).is_ok());
    }

    #[test]
    fn validate_run_transition_valid() {
        assert!(validate_run_transition(RunStatus::Pending, RunStatus::InProgress).is_ok());
        assert!(validate_run_transition(RunStatus::InProgress, RunStatus::Success).is_ok());
        assert!(validate_run_transition(RunStatus::InProgress, RunStatus::Error).is_ok());
        assert!(validate_run_transition(RunStatus::Pending, RunStatus::Success).is_ok());
        assert!(validate_run_transition(RunStatus::Pending, RunStatus::Error).is_ok());
    }

    #[test]
    fn validate_run_transition_invalid() {
        assert!(validate_run_transition(RunStatus::Success, RunStatus::InProgress).is_err());
        assert!(validate_run_transition(RunStatus::Error, RunStatus::Pending).is_err());
        assert!(validate_run_transition(RunStatus::Success, RunStatus::Error).is_err());
    }

    #[test]
    fn update_expiration_positive_minutes() {
        let result = update_expiration(Some(30));
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn update_expiration_zero_minutes() {
        let result = update_expiration(Some(0));
        assert!(result.is_err());
    }

    #[test]
    fn update_expiration_negative_minutes() {
        let result = update_expiration(Some(-5));
        assert!(result.is_err());
    }

    #[test]
    fn update_expiration_none() {
        let result = update_expiration(None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn watcher_restart_needed_true() {
        let mut params = TaskUpdateParams {
            id: "test".to_string(),
            prompt: None,
            cli: None,
            model: None,
            schedule: None,
            working_dir: None,
            duration_minutes: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
            enabled: None,
        };
        assert!(!watcher_restart_needed(&params));

        params.path = Some("/tmp/test".to_string());
        assert!(watcher_restart_needed(&params));

        params.path = None;
        params.events = Some(vec!["modify".to_string()]);
        assert!(watcher_restart_needed(&params));

        params.events = None;
        params.debounce_seconds = Some(5);
        assert!(watcher_restart_needed(&params));

        params.debounce_seconds = None;
        params.recursive = Some(true);
        assert!(watcher_restart_needed(&params));

        params.recursive = None;
        params.cli = Some("claude".to_string());
        assert!(watcher_restart_needed(&params));

        params.cli = None;
        params.prompt = Some("new prompt".to_string());
        assert!(watcher_restart_needed(&params));

        params.prompt = None;
        params.model = Some(Some("claude-4".to_string()));
        assert!(watcher_restart_needed(&params));
    }

    #[test]
    fn new_agent_base_sets_defaults() {
        let agent = new_agent_base(
            "test-id".to_string(),
            "test prompt".to_string(),
            Cli::new("opencode"),
            None,
            None,
            None,
            "/tmp/test.log".to_string(),
        );

        assert_eq!(agent.id, "test-id");
        assert_eq!(agent.prompt, "test prompt");
        assert_eq!(agent.cli.as_str(), "opencode");
        assert!(agent.model.is_none());
        assert!(agent.working_dir.is_none());
        assert!(agent.enabled);
        assert_eq!(agent.timeout_minutes, 15);
        assert!(agent.expires_at.is_none());
        assert!(agent.trigger.is_none());
        assert_eq!(agent.log_path, "/tmp/test.log");
    }

    #[test]
    fn new_agent_base_custom_timeout() {
        let agent = new_agent_base(
            "test-id".to_string(),
            "test".to_string(),
            Cli::new("opencode"),
            None,
            None,
            Some(30),
            "/tmp/test.log".to_string(),
        );
        assert_eq!(agent.timeout_minutes, 30);
    }

    #[test]
    fn apply_scalar_updates_prompt() {
        let mut agent = Agent {
            id: "test".to_string(),
            prompt: "old".to_string(),
            trigger: None,
            cli: Cli::new("opencode"),
            model: None,
            working_dir: None,
            enabled: true,
            created_at: Utc::now(),
            log_path: "/tmp/test.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        };

        let params = TaskUpdateParams {
            id: "test".to_string(),
            prompt: Some("new prompt".to_string()),
            cli: None,
            model: None,
            schedule: None,
            working_dir: None,
            duration_minutes: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
            enabled: None,
        };

        apply_scalar_updates(&mut agent, &params).unwrap();
        assert_eq!(agent.prompt, "new prompt");
    }

    #[test]
    fn apply_scalar_updates_enabled_toggle() {
        let mut agent = Agent {
            id: "test".to_string(),
            prompt: "test".to_string(),
            trigger: None,
            cli: Cli::new("opencode"),
            model: None,
            working_dir: None,
            enabled: true,
            created_at: Utc::now(),
            log_path: "/tmp/test.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        };

        let params = TaskUpdateParams {
            id: "test".to_string(),
            prompt: None,
            cli: None,
            model: None,
            schedule: None,
            working_dir: None,
            duration_minutes: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
            enabled: Some(false),
        };

        apply_scalar_updates(&mut agent, &params).unwrap();
        assert!(!agent.enabled);
    }

    #[test]
    fn apply_scalar_updates_empty_params_no_change() {
        let mut agent = Agent {
            id: "test".to_string(),
            prompt: "original".to_string(),
            trigger: None,
            cli: Cli::new("opencode"),
            model: Some("model-1".to_string()),
            working_dir: Some("/original".to_string()),
            enabled: true,
            created_at: Utc::now(),
            log_path: "/tmp/test.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        };

        let params = TaskUpdateParams {
            id: "test".to_string(),
            prompt: None,
            cli: None,
            model: None,
            schedule: None,
            working_dir: None,
            duration_minutes: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
            enabled: None,
        };

        apply_scalar_updates(&mut agent, &params).unwrap();
        assert_eq!(agent.prompt, "original");
        assert_eq!(agent.model, Some("model-1".to_string()));
        assert_eq!(agent.working_dir, Some("/original".to_string()));
        assert!(agent.enabled);
    }

    #[test]
    fn map_action_result_error() {
        let result: Result<(), String> = Err("something failed".to_string());
        let tool_result = map_action_result(result, "done");
        // Error results have is_error = Some(true)
        assert_eq!(tool_result.is_error, Some(true));
    }

    #[test]
    fn map_action_result_success() {
        let result: Result<(), String> = Ok(());
        let tool_result = map_action_result(result, "done");
        // Success results have is_error = None or Some(false)
        assert_ne!(tool_result.is_error, Some(true));
    }

    #[test]
    fn prepare_cron_task_valid() {
        let params = TaskAddParams {
            id: "test-agent".to_string(),
            prompt: "Run tests".to_string(),
            schedule: "0 9 * * *".to_string(),
            cli: Some("opencode".to_string()),
            model: None,
            duration_minutes: None,
            working_dir: None,
            timeout_minutes: None,
        };

        let result = prepare_cron_task(&params, &|_| true);
        assert!(result.is_ok());
        let prepared = result.unwrap();
        assert_eq!(prepared.schedule_expr, "0 9 * * *");
        assert_eq!(prepared.cli.as_str(), "opencode");
    }

    #[test]
    fn prepare_cron_task_invalid_id() {
        let params = TaskAddParams {
            id: "".to_string(),
            prompt: "Run tests".to_string(),
            schedule: "0 9 * * *".to_string(),
            cli: None,
            model: None,
            duration_minutes: None,
            working_dir: None,
            timeout_minutes: None,
        };

        let result = prepare_cron_task(&params, &|_| true);
        assert!(result.is_err());
    }

    #[test]
    fn prepare_cron_task_invalid_schedule() {
        let params = TaskAddParams {
            id: "test-agent".to_string(),
            prompt: "Run tests".to_string(),
            schedule: "not a cron".to_string(),
            cli: None,
            model: None,
            duration_minutes: None,
            working_dir: None,
            timeout_minutes: None,
        };

        let result = prepare_cron_task(&params, &|_| false);
        assert!(result.is_err());
    }

    #[test]
    fn prepare_watch_task_valid() {
        let params = TaskWatchParams {
            id: "test-watcher".to_string(),
            path: "/tmp/test".to_string(),
            events: vec!["create".to_string(), "modify".to_string()],
            prompt: "Check files".to_string(),
            cli: Some("claude".to_string()),
            model: None,
            debounce_seconds: Some(5),
            recursive: Some(true),
            timeout_minutes: None,
        };

        let result = prepare_watch_task(&params);
        assert!(result.is_ok());
        let prepared = result.unwrap();
        assert_eq!(prepared.cli.as_str(), "claude");
        assert_eq!(prepared.debounce_seconds, 5);
        assert!(prepared.recursive);
        assert_eq!(prepared.events.len(), 2);
    }

    #[test]
    fn prepare_watch_task_defaults() {
        let params = TaskWatchParams {
            id: "test-watcher".to_string(),
            path: "/tmp/test".to_string(),
            events: vec!["modify".to_string()],
            prompt: "Check files".to_string(),
            cli: Some("opencode".to_string()),
            model: None,
            debounce_seconds: None,
            recursive: None,
            timeout_minutes: None,
        };

        let prepared = prepare_watch_task(&params).expect("prepare_watch_task should succeed");
        assert_eq!(prepared.debounce_seconds, 2); // default
        assert!(!prepared.recursive); // default
    }

    #[test]
    fn prepare_watch_task_invalid_path() {
        let params = TaskWatchParams {
            id: "test-watcher".to_string(),
            path: "".to_string(),
            events: vec!["modify".to_string()],
            prompt: "Check files".to_string(),
            cli: None,
            model: None,
            debounce_seconds: None,
            recursive: None,
            timeout_minutes: None,
        };

        let result = prepare_watch_task(&params);
        assert!(result.is_err());
    }
}
