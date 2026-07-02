//! Unit tests for executor module

use crate::application::notification_service::{DefaultNotificationService, NotificationService};
use crate::application::ports::StateRepository;
use crate::db::Database;
use std::sync::Arc;
use tempfile::tempdir;

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
