//! Unit tests for executor module

use crate::application::notification_service::{DefaultNotificationService, NotificationService};
use crate::application::ports::{AgentRepository, RunRepository, StateRepository};
use crate::db::Database;
use crate::domain::models::{Agent, Cli};
use crate::executor::Executor;
use chrono::Utc;
use std::sync::Arc;
use tempfile::tempdir;

fn agent_with_unresolvable_cli(id: &str, log_path: &std::path::Path) -> Agent {
    Agent {
        id: id.to_string(),
        prompt: "do nothing".to_string(),
        trigger: None,
        cli: Cli::new("definitely-not-a-real-cli-binary-xyz"),
        model: None,
        working_dir: None,
        enabled: true,
        enable_at: None,
        created_at: Utc::now(),
        log_path: log_path.to_string_lossy().to_string(),
        timeout_minutes: 15,
        expires_at: None,
        last_run_at: None,
        last_run_ok: None,
        last_triggered_at: None,
        trigger_count: 0,
    }
}

#[test]
fn test_database_state_operations() {
    // Test basic database state operations through executor's database
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Arc::new(Database::new(&db_path).unwrap());

    // Test setting and getting state
    assert!(db.set_state("executor_test", "test_value").is_ok());
    let result = db.get_state("executor_test").unwrap();
    assert_eq!(result, Some("test_value".to_string()));
}

#[test]
fn test_notification_service_integration() {
    // Test that notification service can be used
    let service = DefaultNotificationService;

    // These methods should work without panicking
    service.notify_task_failed("test-agent", 1, "test error");
    service.notify_agent_failed("test-agent", "opencode", 1, "test output");
    service.notify_task_completed("test-agent", true, Some(0));
    service.notify_nursery_failed("test nursery error");
}

#[test]
fn wrap_prompt_uses_agent_report_tool_name() {
    let out = super::wrap_prompt("do the thing", "agent-abc", "run-xyz");
    assert!(
        out.contains("agent_report(run_id=\"run-xyz\""),
        "expected in_progress agent_report call, got:\n{out}"
    );
    assert!(out.contains("agent_report(run_id=\"run-xyz\", status=\"success\""));
    assert!(out.contains("agent_report(run_id=\"run-xyz\", status=\"error\""));
    assert!(
        !out.contains("task_report"),
        "wrap_prompt must not reference the old/incorrect task_report name"
    );
    assert!(out.contains("Agent ID: agent-abc"));
    assert!(out.contains("Run ID: run-xyz"));
    assert!(out.contains("[USER TASK]"));
    assert!(out.contains("do the thing"));
}

/// Records which notification methods fired, so the failure-first policy
/// (B27) can be asserted without sending a real desktop toast.
#[derive(Default)]
struct RecordingNotifier {
    events: std::sync::Mutex<Vec<String>>,
}

impl RecordingNotifier {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
    fn push(&self, e: &str) {
        self.events.lock().unwrap().push(e.to_string());
    }
}

impl NotificationService for RecordingNotifier {
    fn notify_task_completed(&self, id: &str, _success: bool, _exit_code: Option<i32>) {
        self.push(&format!("completed:{id}"));
    }
    fn notify_task_failed(&self, id: &str, _exit_code: i32, _error_msg: &str) {
        self.push(&format!("failed:{id}"));
    }
    fn notify_watcher_triggered(&self, _watcher_id: &str, _path: &str, _event: &str) {}
    fn notify_agent_failed(&self, id: &str, _cli: &str, _exit_code: i32, _output: &str) {
        self.push(&format!("agent_failed:{id}"));
    }
    fn notify_nursery_failed(&self, _error_msg: &str) {}
    fn notify_loop_started(
        &self,
        _loop_name: &str,
        _spec_count: usize,
        _resumed: bool,
        _first_pending: Option<&str>,
    ) {
    }
    fn notify_spec_completed(
        &self,
        _loop_name: &str,
        _spec_name: &str,
        _done: usize,
        _total: usize,
        _next_pending: Option<&str>,
    ) {
    }
    fn notify_loop_finished(
        &self,
        _loop_name: &str,
        _outcome: crate::application::notification_service::LoopFinishOutcome<'_>,
    ) {
    }
    fn notify_loop_completion_hook_failed(&self, _loop_name: &str, _error: &str) {}
}

fn cron_agent(id: &str) -> Agent {
    Agent {
        id: id.to_string(),
        prompt: "do nothing".to_string(),
        trigger: Some(crate::domain::models::Trigger::Cron {
            schedule_expr: "*/15 * * * *".to_string(),
        }),
        cli: Cli::new("opencode"),
        model: None,
        working_dir: None,
        enabled: true,
        enable_at: None,
        created_at: Utc::now(),
        log_path: "/tmp/x.log".to_string(),
        timeout_minutes: 15,
        expires_at: None,
        last_run_at: None,
        last_run_ok: None,
        last_triggered_at: None,
        trigger_count: 0,
    }
}

#[test]
fn success_policy_scheduled_run_stays_silent_unless_opted_in() {
    let dir = tempdir().unwrap();
    let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
    let agent = cron_agent("cron-quiet");
    db.upsert_agent(&agent).unwrap();

    let notifier = Arc::new(RecordingNotifier::default());
    let executor = Executor::new(db.clone(), notifier.clone());
    let ok = super::CliRunResult {
        exit_code: 0,
        success: true,
    };

    // Scheduled success, not opted in, not manual → silent.
    executor.notify_result(&agent, &ok, false, false);
    assert!(
        notifier.events().is_empty(),
        "a scheduled successful run must not toast by default"
    );

    // Opt in → success now toasts.
    db.set_agent_notify_on_success(&agent.id, true).unwrap();
    executor.notify_result(&agent, &ok, false, false);
    assert_eq!(notifier.events(), vec!["completed:cron-quiet".to_string()]);
}

#[test]
fn success_policy_manual_run_always_toasts_and_failures_always_toast() {
    let dir = tempdir().unwrap();
    let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
    let agent = cron_agent("cron-agent");
    db.upsert_agent(&agent).unwrap();

    let notifier = Arc::new(RecordingNotifier::default());
    let executor = Executor::new(db, notifier.clone());
    let ok = super::CliRunResult {
        exit_code: 0,
        success: true,
    };
    let fail = super::CliRunResult {
        exit_code: 2,
        success: false,
    };

    // Manual success → always toasts even without opt-in.
    executor.notify_result(&agent, &ok, false, true);
    // Scheduled failure → always toasts.
    executor.notify_result(&agent, &fail, false, false);
    // Watch failure → always toasts (agent_failed variant).
    executor.notify_result(&agent, &fail, true, false);

    assert_eq!(
        notifier.events(),
        vec![
            "completed:cron-agent".to_string(),
            "failed:cron-agent".to_string(),
            "agent_failed:cron-agent".to_string(),
        ]
    );
}

#[test]
fn notify_on_success_defaults_false_and_survives_agent_upsert() {
    let dir = tempdir().unwrap();
    let db = Database::new(&dir.path().join("test.db")).unwrap();
    let mut agent = cron_agent("survives");
    db.upsert_agent(&agent).unwrap();

    assert!(
        !db.agent_notify_on_success(&agent.id).unwrap(),
        "flag must default to false"
    );
    assert!(
        !db.agent_notify_on_success("no-such-agent").unwrap(),
        "a missing agent reads as false, never an error"
    );

    db.set_agent_notify_on_success(&agent.id, true).unwrap();
    // Re-upserting the agent (as agent_update does) must not wipe the flag.
    agent.prompt = "changed".to_string();
    db.upsert_agent(&agent).unwrap();
    assert!(
        db.agent_notify_on_success(&agent.id).unwrap(),
        "notify_on_success must survive an agent edit"
    );
}

#[tokio::test]
async fn unresolvable_cli_binary_does_not_leave_run_locked_forever() {
    let dir = tempdir().unwrap();
    let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
    let agent = agent_with_unresolvable_cli("missing-cli-agent", &dir.path().join("agent.log"));
    db.upsert_agent(&agent).unwrap();

    let executor = Executor::new(db.clone(), Arc::new(DefaultNotificationService));

    let exit_code = executor.execute_agent(&agent, true).await.unwrap();
    assert_eq!(
        exit_code, -1,
        "unresolvable CLI must report a failed run, not hang mid-flight"
    );

    let active = db.get_active_run(&agent.id).unwrap();
    assert!(
        active.is_none(),
        "run must be finalized (not left pending/in_progress) when the CLI binary can't be resolved"
    );

    // A subsequent run must acquire the lock normally, with no manual disable/enable needed.
    let exit_code_2 = executor.execute_agent(&agent, true).await.unwrap();
    assert_eq!(exit_code_2, -1);
    assert!(db.get_active_run(&agent.id).unwrap().is_none());
}
