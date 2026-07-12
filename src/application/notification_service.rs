//! Notification service — centralized notification dispatch.
//!
//! Provides a clean abstraction for sending notifications from both
//! daemon (background tasks) and TUI (interactive agents).

/// How a loop run reached a terminal state, for [`NotificationService::notify_loop_finished`].
pub enum LoopFinishOutcome<'a> {
    /// Every spec in the run reached `completed`.
    Completed { done: usize, total: usize },
    /// A spec failed and the loop has no more retries/routes to take.
    Failed { spec_name: &'a str },
    /// A node reported a blocker needing human intervention; the loop paused.
    Blocked { summary: &'a str },
}

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

    /// Send a notification when a loop run actually begins executing (a
    /// fresh dispatch or a resume alike — anything that starts driving the
    /// loop's graph).
    fn notify_loop_started(&self, loop_name: &str, spec_count: usize);

    /// Send a notification each time a spec within a loop reaches `completed`.
    fn notify_spec_completed(&self, loop_name: &str, spec_name: &str, done: usize, total: usize);

    /// Send a notification when a loop run reaches a terminal state
    /// (completed, failed, or blocked).
    fn notify_loop_finished(&self, loop_name: &str, outcome: LoopFinishOutcome<'_>);
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

    fn notify_loop_started(&self, loop_name: &str, spec_count: usize) {
        let body = format!("Started · {spec_count} specs");
        send_notification(loop_name, &body, NotificationLevel::Info);
    }

    fn notify_spec_completed(&self, loop_name: &str, spec_name: &str, done: usize, total: usize) {
        let body = format!("{spec_name} ✓ · {done}/{total}");
        send_notification(loop_name, &body, NotificationLevel::Success);
    }

    fn notify_loop_finished(&self, loop_name: &str, outcome: LoopFinishOutcome<'_>) {
        let (body, level) = match outcome {
            LoopFinishOutcome::Completed { done, total } => (
                format!("Completed · {done}/{total}"),
                NotificationLevel::Success,
            ),
            LoopFinishOutcome::Failed { spec_name } => {
                (format!("Failed · {spec_name}"), NotificationLevel::Error)
            }
            LoopFinishOutcome::Blocked { summary } => {
                (format!("Blocked · {summary}"), NotificationLevel::Warning)
            }
        };
        send_notification(loop_name, &body, level);
    }
}
