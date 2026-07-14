//! Unit tests for the application layer

use crate::application::ports::StateRepository;

#[cfg(test)]
mod test {
    use super::*;
    use crate::application::notification_service::{
        DefaultNotificationService, LoopFinishOutcome, NotificationService,
    };
    use crate::db::Database;
    use tempfile::tempdir;

    #[test]
    fn test_database_state_operations() {
        // Test basic database state operations
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::new(&db_path).unwrap();

        // Test set and get state
        assert!(db.set_state("test-key", "test-value").is_ok());
        let result = db.get_state("test-key").unwrap();
        assert_eq!(result, Some("test-value".to_string()));

        // Test getting missing state
        let result = db.get_state("missing-key").unwrap();
        assert!(result.is_none());

        // Test overwriting state
        assert!(db.set_state("test-key", "new-value").is_ok());
        let result = db.get_state("test-key").unwrap();
        assert_eq!(result, Some("new-value".to_string()));
    }

    #[test]
    fn test_notification_service_methods() {
        // Test that notification service trait methods work
        let service = DefaultNotificationService;

        // Test task failed notification (returns ())
        service.notify_task_failed("test-agent", 1, "error occurred");

        // Test agent failed notification (returns ())
        service.notify_agent_failed("test-agent", "opencode", 1, "error output");

        // Test task completed notification
        service.notify_task_completed("test-agent", true, Some(0));

        // Test nursery failed notification
        service.notify_nursery_failed("identity.toml not found");

        // Test loop lifecycle notifications (return ())
        service.notify_loop_started("R4 loop", 19);
        service.notify_spec_completed("R4 loop", "R4", 11, 19);
        service.notify_loop_finished(
            "R4 loop",
            LoopFinishOutcome::Completed {
                done: 19,
                total: 19,
                hook_launched: false,
            },
        );
        service.notify_loop_finished("R4 loop", LoopFinishOutcome::Failed { spec_name: "R4" });
        service.notify_loop_finished(
            "R4 loop",
            LoopFinishOutcome::Blocked {
                summary: "needs human review",
            },
        );

        // N2: post-completion hook failure notification.
        service
            .notify_loop_completion_hook_failed("R4 loop", "on_completed hook exited with code 1.");
    }
}
