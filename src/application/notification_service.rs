//! Notification service — centralized notification dispatch.
//!
//! Provides a clean abstraction for sending notifications from both
//! daemon (background tasks) and TUI (interactive agents).

/// How a loop run reached a terminal state, for [`NotificationService::notify_loop_finished`].
pub enum LoopFinishOutcome<'a> {
    /// Every spec in the run reached `completed`.
    Completed {
        done: usize,
        total: usize,
        /// Whether this run's `on_completed` hook (N2) was launched right
        /// after this notification — surfaced in the notification body so a
        /// human watching it knows a post-completion agent is now running.
        hook_launched: bool,
    },
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

    /// Send a notification when a loop run actually begins executing.
    ///
    /// `resumed` distinguishes a fresh launch ("Started") from picking up
    /// where a prior run left off ("Resumed") — a reset+rerun or an autorun
    /// resume shouldn't read as the loop starting from scratch again.
    /// `first_pending` names the spec this dispatch will work first, when
    /// known.
    fn notify_loop_started(
        &self,
        loop_name: &str,
        spec_count: usize,
        resumed: bool,
        first_pending: Option<&str>,
    );

    /// Send a notification each time a spec within a loop reaches `completed`.
    /// `next_pending` names the spec that will run next (if any), so the toast
    /// says both what just finished and what's coming.
    fn notify_spec_completed(
        &self,
        loop_name: &str,
        spec_name: &str,
        done: usize,
        total: usize,
        next_pending: Option<&str>,
    );

    /// Send a notification when a loop run reaches a terminal state
    /// (completed, failed, or blocked).
    fn notify_loop_finished(&self, loop_name: &str, outcome: LoopFinishOutcome<'_>);

    /// Send a notification when a loop's `on_completed` hook (N2) fails —
    /// a bad platform/CLI config, a non-zero exit, or a timeout. Never sent
    /// for the loop run itself (that already finished successfully by the
    /// time the hook runs); this is purely about the hook's own outcome.
    fn notify_loop_completion_hook_failed(&self, loop_name: &str, error: &str);

    fn notify_announcement(&self, title: &str, body: &str);
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

    fn notify_loop_started(
        &self,
        loop_name: &str,
        spec_count: usize,
        resumed: bool,
        first_pending: Option<&str>,
    ) {
        let verb = if resumed { "Resumed" } else { "Started" };
        let next = first_pending
            .map(|name| format!(" · next: {name}"))
            .unwrap_or_default();
        let body = format!("{verb} · {spec_count} specs{next}");
        send_notification(loop_name, &body, NotificationLevel::Info);
    }

    fn notify_spec_completed(
        &self,
        loop_name: &str,
        spec_name: &str,
        done: usize,
        total: usize,
        next_pending: Option<&str>,
    ) {
        let next = next_pending
            .map(|name| format!(" · next: {name}"))
            .unwrap_or_default();
        let body = format!("{spec_name} ✓ · {done}/{total}{next}");
        send_notification(loop_name, &body, NotificationLevel::Success);
    }

    fn notify_loop_finished(&self, loop_name: &str, outcome: LoopFinishOutcome<'_>) {
        let (body, level) = match outcome {
            LoopFinishOutcome::Completed {
                done,
                total,
                hook_launched,
            } => {
                let hook_note = if hook_launched {
                    " · post-completion hook launched"
                } else {
                    ""
                };
                (
                    format!("Completed · {done}/{total}{hook_note}"),
                    NotificationLevel::Success,
                )
            }
            LoopFinishOutcome::Failed { spec_name } => {
                (format!("Failed · {spec_name}"), NotificationLevel::Error)
            }
            LoopFinishOutcome::Blocked { summary } => {
                (format!("Blocked · {summary}"), NotificationLevel::Warning)
            }
        };
        send_notification(loop_name, &body, level);
    }

    fn notify_loop_completion_hook_failed(&self, loop_name: &str, error: &str) {
        send_notification(
            loop_name,
            &format!("Post-completion hook failed · {error}"),
            NotificationLevel::Warning,
        );
    }

    fn notify_announcement(&self, title: &str, body: &str) {
        send_notification(title, body, NotificationLevel::Info);
    }
}
