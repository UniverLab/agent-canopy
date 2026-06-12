//! Notification service — centralized notification dispatch.
//!
//! Provides a clean abstraction for sending notifications from both
//! daemon (background tasks) and TUI (interactive agents).

/// Notification service for sending cross-platform desktop notifications.
pub trait NotificationService: Send + Sync {
    /// Send a notification about a completing background task.
    fn notify_task_completed(&self, task_id: &str, success: bool, exit_code: Option<i32>);

    /// Send a notification about a failed background task.
    fn notify_task_failed(&self, task_id: &str, exit_code: i32, error_msg: &str);

    /// Send a notification about a completed watcher trigger.
    #[allow(dead_code)]
    fn notify_watcher_triggered(&self, watcher_id: &str, path: &str, event: &str);

    /// Send a notification about an interactive agent failure.
    fn notify_agent_failed(&self, agent_id: &str, cli: &str, exit_code: i32, output: &str);

    /// Send a notification about a nursery (seed creation) failure.
    fn notify_nursery_failed(&self, error_msg: &str);
}

/// Default notification service implementation using domain notification module.
#[derive(Debug, Default)]
pub struct DefaultNotificationService;

impl NotificationService for DefaultNotificationService {
    fn notify_task_completed(&self, task_id: &str, success: bool, exit_code: Option<i32>) {
        let title = "Canopy — task finished";
        let body = if success {
            format!("{task_id} completed successfully")
        } else if let Some(code) = exit_code {
            format!("{task_id} completed with exit code {code}")
        } else {
            format!("{task_id} completed")
        };
        crate::domain::notification::send_notification(title, &body);
    }

    fn notify_task_failed(&self, task_id: &str, exit_code: i32, error_msg: &str) {
        let title = "Canopy — task failed";
        let body = if error_msg.is_empty() {
            format!("{task_id} failed with exit code {exit_code}")
        } else {
            format!("{task_id} failed ({exit_code}): {error_msg}")
        };
        crate::domain::notification::send_notification(title, &body);
    }

    fn notify_watcher_triggered(&self, watcher_id: &str, path: &str, event: &str) {
        let title = "Canopy — file change detected";
        let body = format!("{watcher_id}: {event} at {path}");
        crate::domain::notification::send_notification(title, &body);
    }

    fn notify_agent_failed(&self, agent_id: &str, cli: &str, exit_code: i32, output: &str) {
        let title = "Canopy — agent failed";
        let body = if output.is_empty() {
            format!("{agent_id} ({cli}) exited with code {exit_code}")
        } else {
            format!("{agent_id} ({cli}) exited ({exit_code})\n{output}")
        };
        crate::domain::notification::send_notification(title, &body);
    }

    fn notify_nursery_failed(&self, error_msg: &str) {
        let title = "Canopy — seed creation failed";
        let body = format!("Nursery finalization failed: {error_msg}");
        crate::domain::notification::send_notification(title, &body);
    }
}
