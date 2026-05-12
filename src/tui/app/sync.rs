use crate::domain::sync::summarize_sync_context;
use crate::tui::agent::AgentStatus;

use super::types::{AgentEntry, App, SyncPanelState};

pub(crate) const SYNC_PANEL_WIDTH: u16 = 34;
const MIN_PANEL_WIDTH: u16 = 90;
const RECENT_MESSAGE_LIMIT: usize = 18;
const CHATTER_LIMIT: usize = 8;

impl App {
    /// Whether sync is available for the currently selected session,
    /// regardless of whether the panel is currently visible.
    pub(crate) fn sync_available(&self) -> bool {
        let Some(workdir) = self.selected_sync_workdir() else {
            return false;
        };
        self.live_session_count_for_workdir(workdir) >= 2
    }

    /// Returns active missions for a workdir without the ≥2 session gate.
    ///
    /// Used by the system prompt so missions are always visible, even in solo
    /// mode. This is intentional: solo agents benefit from seeing prior mission
    /// history and stale missions that need cleanup.
    pub(crate) fn active_missions_for_workdir(
        &self,
        workdir: &str,
    ) -> Vec<crate::domain::sync::ActiveIntent> {
        let Ok(messages) = self.db.list_sync_messages(workdir, RECENT_MESSAGE_LIMIT) else {
            return Vec::new();
        };
        let active_agent_ids = messages
            .iter()
            .map(|m| m.agent_id.clone())
            .collect::<std::collections::HashSet<_>>();
        summarize_sync_context(&messages, &active_agent_ids, 0).active_intents
    }

    pub(crate) fn sync_panel_state(&self) -> Option<SyncPanelState> {
        if !self.sync_panel_visible {
            return None;
        }
        if !self.sync_available() {
            return None;
        }

        let workdir = self.selected_sync_workdir()?;
        let recent_messages = self
            .db
            .list_sync_messages(workdir, RECENT_MESSAGE_LIMIT)
            .ok()?;
        // Derive active agent IDs directly from the messages — the agent_id in sync
        // messages is set by the external CLI (e.g. "copilot-cli") and cannot be
        // reliably mapped to Canopy's internal InteractiveAgent IDs or display names.
        // Showing all agents that have posted in the recent window is the right UX.
        let active_agent_ids = recent_messages
            .iter()
            .map(|m| m.agent_id.clone())
            .collect::<std::collections::HashSet<_>>();
        let summary = summarize_sync_context(&recent_messages, &active_agent_ids, CHATTER_LIMIT);

        Some(SyncPanelState {
            workdir: workdir.to_owned(),
            participant_count: self.live_session_count_for_workdir(workdir),
            vibe: summary.vibe,
            active_intents: summary.active_intents,
            recent_messages,
        })
    }

    pub(crate) fn sync_panel_layout_width(&self, total_width: u16, enabled: bool) -> u16 {
        if !enabled || total_width < MIN_PANEL_WIDTH {
            return 0;
        }

        SYNC_PANEL_WIDTH.min(total_width.saturating_sub(48))
    }

    fn selected_sync_workdir(&self) -> Option<&str> {
        match self.selected_agent()? {
            AgentEntry::Interactive(idx) => self
                .interactive_agents
                .get(*idx)
                .filter(|agent| agent.status == AgentStatus::Running)
                .map(|agent| agent.working_dir.as_str()),
            AgentEntry::Terminal(_) | AgentEntry::Agent(_) | AgentEntry::Group(_) => None,
        }
    }

    fn live_session_count_for_workdir(&self, workdir: &str) -> usize {
        self.interactive_agents
            .iter()
            .filter(|agent| agent.status == AgentStatus::Running && agent.working_dir == workdir)
            .count()
    }
}
