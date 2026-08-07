//! Loop run-time controls — run/pause/continue/reset/autorun for the loop
//! currently focused in the TUI's live view.
//!
//! Every action dispatches through the daemon's MCP tools via
//! [`crate::tui::mcp_client::call_daemon_tool`] — the exact same
//! daemon-delegating path `daemon/loop_cli.rs` uses for the CLI's
//! `loop run`/`pause`/`continue`/`reset`/`autorun` subcommands — never the
//! database directly. The call itself runs on a background thread so the UI
//! thread never blocks on the daemon's HTTP round-trip; [`App::poll_loop_action`]
//! picks up the result non-blockingly on the next tick, exactly like
//! `App::poll_playground_search`'s `mpsc` pattern.

use crate::application::ports::StateRepository;
use crate::domain::loops::LoopStatus;
use crate::tui::app::types::App;
use crate::tui::mcp_client;

/// Outcome of a background loop-control dispatch, delivered to the main
/// thread via `App::loop_action_rx` and applied by [`App::poll_loop_action`].
pub(crate) struct LoopActionOutcome {
    pub is_error: bool,
    pub text: String,
}

/// Last-shown result of a loop control action — the daemon's response,
/// verbatim, whether it succeeded or was refused.
pub(crate) struct LoopActionMessage {
    pub is_error: bool,
    pub text: String,
}

/// How long a loop-action result banner stays up before auto-dismissing
/// (mirrors `show_copied`/`dismiss_copied`'s 2s, held a bit longer since
/// these messages are often full daemon sentences, not a one-word toast).
const LOOP_ACTION_MESSAGE_TTL: std::time::Duration = std::time::Duration::from_secs(6);

/// One control offered for a loop in a given [`LoopStatus`] — the source of
/// truth for both the footer's discoverability hints and the key handler's
/// gating, so the two can never disagree about what's currently valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopControlAction {
    Run,
    Pause,
    ContinueRetry,
    ContinueSkip,
    Reset,
}

impl LoopControlAction {
    pub(crate) fn key(self) -> &'static str {
        match self {
            LoopControlAction::Run => "r",
            LoopControlAction::Pause => "p",
            LoopControlAction::ContinueRetry => "c",
            LoopControlAction::ContinueSkip => "C",
            LoopControlAction::Reset => "x",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            LoopControlAction::Run => "run",
            LoopControlAction::Pause => "pause",
            LoopControlAction::ContinueRetry => "continue (retry)",
            LoopControlAction::ContinueSkip => "continue (skip spec)",
            LoopControlAction::Reset => "reset",
        }
    }
}

/// Controls valid for a loop currently in `status` — a running loop offers
/// only pause; a paused one offers both continue modes (retrying the
/// in-flight node vs. abandoning the current spec are not interchangeable,
/// so both are always presented together); a completed or failed one offers
/// reset and run; a draft (never launched) loop offers only run.
pub(crate) fn available_loop_actions(status: LoopStatus) -> Vec<LoopControlAction> {
    match status {
        LoopStatus::Running => vec![LoopControlAction::Pause],
        LoopStatus::Paused => vec![
            LoopControlAction::ContinueRetry,
            LoopControlAction::ContinueSkip,
        ],
        LoopStatus::Completed | LoopStatus::Failed => {
            vec![LoopControlAction::Reset, LoopControlAction::Run]
        }
        LoopStatus::Draft => vec![LoopControlAction::Run],
    }
}

/// Free-text autorun scheduling input for the loop currently focused in the
/// live view. A single field, mirroring the CLI's `--at`/`--quota-reset-message`
/// split collapsed into one: an empty submission cancels any pending autorun,
/// a value that parses as an RFC 3339 instant is sent as `at`, anything else
/// is sent as the raw `quota_reset_message` for the daemon to parse (the
/// engine, never the TUI, computes the resulting instant).
pub(crate) struct LoopAutorunDialog {
    pub loop_id: String,
    pub loop_name: String,
    pub input: String,
}

impl LoopAutorunDialog {
    pub fn new(loop_id: String, loop_name: String) -> Self {
        Self {
            loop_id,
            loop_name,
            input: String::new(),
        }
    }
}

impl App {
    /// The loop these run-time controls act on: the selected loop, but only
    /// from the live (non-archived) list — an archived loop is inert until
    /// restored, so every control here is a no-op while browsing the
    /// archive.
    fn actionable_selected_loop(&self) -> Option<&crate::domain::loops::Loop> {
        if self.loop_view_archived {
            return None;
        }
        self.selected_loop()
    }

    /// Dispatch one loop-control MCP tool call on a background thread. A
    /// no-op while a previous dispatch is still in flight, so a held key (or
    /// a fast double-press) can't race two calls against the same loop.
    pub(crate) fn dispatch_loop_action(
        &mut self,
        tool: &'static str,
        arguments: serde_json::Value,
    ) {
        if self.loop_action_pending {
            return;
        }
        self.loop_action_message = None;
        let port = self
            .db
            .get_state("port")
            .ok()
            .flatten()
            .unwrap_or_else(|| "7755".to_string());

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = match mcp_client::call_daemon_tool(&port, tool, &arguments) {
                Ok(outcome) => LoopActionOutcome {
                    is_error: outcome.is_error,
                    text: outcome.text,
                },
                Err(e) => LoopActionOutcome {
                    is_error: true,
                    text: format!("Daemon call failed: {e:#}"),
                },
            };
            let _ = tx.send(outcome);
        });
        self.loop_action_pending = true;
        self.loop_action_rx = Some(rx);
    }

    /// Apply a finished background loop-action dispatch, if any. Called from
    /// the tick loop; never blocks (mirrors `App::poll_playground_search`).
    pub(crate) fn poll_loop_action(&mut self) {
        let Some(rx) = &self.loop_action_rx else {
            return;
        };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.loop_action_pending = false;
                self.loop_action_rx = None;
                return;
            }
        };
        self.loop_action_pending = false;
        self.loop_action_rx = None;
        let is_error = outcome.is_error;
        self.loop_action_message = Some(LoopActionMessage {
            is_error,
            text: outcome.text,
        });
        self.loop_action_message_at = std::time::Instant::now();
        if !is_error {
            let _ = self.refresh_loops();
        }
    }

    /// Clear an old loop-action result banner once its TTL has elapsed
    /// (mirrors `App::dismiss_copied`).
    pub(crate) fn dismiss_loop_action_message(&mut self) {
        if self.loop_action_message.is_some()
            && self.loop_action_message_at.elapsed() > LOOP_ACTION_MESSAGE_TTL
        {
            self.loop_action_message = None;
        }
    }

    /// Run the selected loop (`r`) — valid for a `draft`, `completed`, or
    /// `failed` loop (see [`available_loop_actions`]); a no-op otherwise.
    pub fn run_selected_loop(&mut self) {
        let Some(lp) = self.actionable_selected_loop() else {
            return;
        };
        if !available_loop_actions(lp.status).contains(&LoopControlAction::Run) {
            return;
        }
        let loop_id = lp.id.clone();
        self.dispatch_loop_action("loop_run", serde_json::json!({ "loop_id": loop_id }));
    }

    /// Pause the selected loop (`p`) — valid only while it is `running`.
    pub fn pause_selected_loop(&mut self) {
        let Some(lp) = self.actionable_selected_loop() else {
            return;
        };
        if !available_loop_actions(lp.status).contains(&LoopControlAction::Pause) {
            return;
        }
        let loop_id = lp.id.clone();
        self.dispatch_loop_action("loop_pause", serde_json::json!({ "loop_id": loop_id }));
    }

    /// Continue the selected paused loop, retrying the node that was
    /// mid-flight when it paused (`c`).
    pub fn continue_selected_loop_retry(&mut self) {
        self.continue_selected_loop(LoopControlAction::ContinueRetry, "retry_current_node");
    }

    /// Continue the selected paused loop, abandoning the current spec and
    /// moving on to the next one (`C`).
    pub fn continue_selected_loop_skip(&mut self) {
        self.continue_selected_loop(LoopControlAction::ContinueSkip, "skip_next_spec");
    }

    fn continue_selected_loop(&mut self, action: LoopControlAction, daemon_action: &'static str) {
        let Some(lp) = self.actionable_selected_loop() else {
            return;
        };
        if !available_loop_actions(lp.status).contains(&action) {
            return;
        }
        let loop_id = lp.id.clone();
        self.dispatch_loop_action(
            "loop_continue",
            serde_json::json!({ "loop_id": loop_id, "action": daemon_action }),
        );
    }

    /// Open the reset confirmation for the selected loop (`x`) — valid only
    /// for a `completed`/`failed` loop. A no-op otherwise, so the modal never
    /// opens for a loop reset can't act on.
    pub fn open_loop_reset_confirm(&mut self) {
        let Some(lp) = self.actionable_selected_loop() else {
            return;
        };
        if !available_loop_actions(lp.status).contains(&LoopControlAction::Reset) {
            return;
        }
        self.loop_reset_confirm = true;
    }

    /// Dispatch `loop_reset` for the selected loop after the confirm modal's
    /// `y`/Enter. Called only while `loop_reset_confirm` is set, so the
    /// selected loop is whatever `open_loop_reset_confirm` validated.
    pub fn confirm_reset_selected_loop(&mut self) {
        let Some(lp) = self.actionable_selected_loop() else {
            return;
        };
        let loop_id = lp.id.clone();
        self.dispatch_loop_action("loop_reset", serde_json::json!({ "loop_id": loop_id }));
    }

    /// Open the autorun-scheduling input for the loop currently focused in
    /// the live view (`a`). Available regardless of the loop's status — the
    /// underlying `loop_schedule_autorun` tool imposes no status guard
    /// (cancelling in particular must work "regardless of the loop's current
    /// status", per its own description).
    pub fn open_loop_autorun_dialog(&mut self) {
        let Some(lp) = self.actionable_selected_loop() else {
            return;
        };
        self.loop_autorun_dialog = Some(LoopAutorunDialog::new(lp.id.clone(), lp.name.clone()));
    }

    /// Close the autorun dialog without submitting.
    pub fn close_loop_autorun_dialog(&mut self) {
        self.loop_autorun_dialog = None;
    }

    /// Submit the autorun dialog's typed input: empty cancels any pending
    /// autorun, an RFC 3339 instant is sent as `at`, anything else is sent
    /// as the raw `quota_reset_message` for the daemon to parse. The TUI
    /// never computes the resulting instant itself.
    pub fn submit_loop_autorun_dialog(&mut self) {
        let Some(dialog) = self.loop_autorun_dialog.take() else {
            return;
        };
        let input = dialog.input.trim();
        let arguments = if input.is_empty() {
            serde_json::json!({ "loop_id": dialog.loop_id })
        } else if chrono::DateTime::parse_from_rfc3339(input).is_ok() {
            serde_json::json!({ "loop_id": dialog.loop_id, "at": input })
        } else {
            serde_json::json!({ "loop_id": dialog.loop_id, "quota_reset_message": input })
        };
        self.dispatch_loop_action("loop_schedule_autorun", arguments);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_actions_running_offers_only_pause() {
        assert_eq!(
            available_loop_actions(LoopStatus::Running),
            vec![LoopControlAction::Pause]
        );
    }

    #[test]
    fn available_actions_paused_offers_both_continue_modes() {
        assert_eq!(
            available_loop_actions(LoopStatus::Paused),
            vec![
                LoopControlAction::ContinueRetry,
                LoopControlAction::ContinueSkip
            ]
        );
    }

    #[test]
    fn available_actions_completed_and_failed_offer_reset_and_run() {
        for status in [LoopStatus::Completed, LoopStatus::Failed] {
            assert_eq!(
                available_loop_actions(status),
                vec![LoopControlAction::Reset, LoopControlAction::Run]
            );
        }
    }

    #[test]
    fn available_actions_draft_offers_only_run() {
        assert_eq!(
            available_loop_actions(LoopStatus::Draft),
            vec![LoopControlAction::Run]
        );
    }

    #[test]
    fn action_keys_are_distinct() {
        let actions = [
            LoopControlAction::Run,
            LoopControlAction::Pause,
            LoopControlAction::ContinueRetry,
            LoopControlAction::ContinueSkip,
            LoopControlAction::Reset,
        ];
        let keys: std::collections::HashSet<&str> = actions.iter().map(|a| a.key()).collect();
        assert_eq!(keys.len(), actions.len());
    }
}
