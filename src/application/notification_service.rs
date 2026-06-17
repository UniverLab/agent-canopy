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

use crate::domain::notification::{send_notification, NotificationLevel};

/// Default notification service implementation using domain notification module.
///
/// The OS already labels every notification as "Canopy" (app name on Linux,
/// AUMID on WSL), so the title carries the *subject* (task/agent/watcher) and
/// the body the outcome. Severity drives a native themed icon on Linux.
#[derive(Debug, Default)]
pub struct DefaultNotificationService;

impl NotificationService for DefaultNotificationService {
    fn notify_task_completed(&self, task_id: &str, success: bool, exit_code: Option<i32>) {
        let (body, level) = if success {
            ("Task completed".to_string(), NotificationLevel::Success)
        } else if let Some(code) = exit_code {
            (
                format!("Finished with exit code {code}"),
                NotificationLevel::Warning,
            )
        } else {
            (
                "Finished with errors".to_string(),
                NotificationLevel::Warning,
            )
        };
        send_notification(task_id, &body, level);
    }

    fn notify_task_failed(&self, task_id: &str, exit_code: i32, error_msg: &str) {
        let body = if error_msg.is_empty() {
            format!("Failed · exit {exit_code}")
        } else {
            format!("Failed · exit {exit_code}\n{error_msg}")
        };
        send_notification(task_id, &body, NotificationLevel::Error);
    }

    fn notify_watcher_triggered(&self, watcher_id: &str, path: &str, event: &str) {
        let body = format!("{event} · {path}");
        send_notification(watcher_id, &body, NotificationLevel::Info);
    }

    fn notify_agent_failed(&self, agent_id: &str, cli: &str, exit_code: i32, output: &str) {
        let body = if output.is_empty() {
            format!("{cli} stopped · exit {exit_code}")
        } else {
            format!("{cli} stopped · exit {exit_code}\n{output}")
        };
        send_notification(agent_id, &body, NotificationLevel::Error);
    }

    fn notify_nursery_failed(&self, error_msg: &str) {
        send_notification("Seed creation failed", error_msg, NotificationLevel::Error);
    }
}
