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
