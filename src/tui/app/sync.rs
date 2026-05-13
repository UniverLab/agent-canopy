use crate::domain::sync::summarize_sync_context;

use super::types::{AgentEntry, App, SyncPanelState};

pub(crate) const ACTIVITY_PANEL_WIDTH: u16 = 34;
const MIN_PANEL_WIDTH: u16 = 90;
const RECENT_MESSAGE_LIMIT: usize = 18;
const MAX_RECENT_MESSAGE_LIMIT: usize = 200;
const MESSAGE_WINDOW_LINES_PER_STEP: u16 = 6;
const MESSAGE_WINDOW_ITEMS_PER_STEP: usize = 8;
const CHATTER_LIMIT: usize = 8;

impl App {
    /// Whether sync is available for the currently selected session,
    /// regardless of whether the panel is currently visible.
    pub(crate) fn sync_available(&self) -> bool {
        let Some(workdir) = self.selected_activity_workdir() else {
            return false;
        };
        self.live_session_count_for_sync(workdir) >= 2
    }

    pub(crate) fn activity_panel_available(&self) -> bool {
        self.selected_activity_workdir().is_some()
    }

    /// Returns activity for the selected workdir without applying visibility rules.
    pub(crate) fn selected_activity_state(&self) -> Option<SyncPanelState> {
        let workdir = self.selected_activity_workdir()?;
        self.activity_panel_state_for_workdir(workdir)
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

    pub(crate) fn activity_panel_state(&self) -> Option<SyncPanelState> {
        let state = self.selected_activity_state()?;
        (!self.hidden_activity_workdirs.contains(&state.workdir)).then_some(state)
    }

    pub(crate) fn activity_panel_layout_width(&self, total_width: u16, enabled: bool) -> u16 {
        if !enabled || total_width < MIN_PANEL_WIDTH {
            return 0;
        }

        ACTIVITY_PANEL_WIDTH.min(total_width.saturating_sub(48))
    }

    pub(crate) fn selected_activity_workdir(&self) -> Option<&str> {
        match self.selected_agent()? {
            AgentEntry::Interactive(idx) => self
                .interactive_agents
                .get(*idx)
                .map(|agent| agent.working_dir.as_str()),
            AgentEntry::Terminal(idx) => self
                .terminal_agents
                .get(*idx)
                .map(|agent| agent.working_dir.as_str()),
            AgentEntry::Agent(agent) => agent.working_dir.as_deref(),
            AgentEntry::Group(_) => None,
        }
    }

    fn activity_panel_state_for_workdir(&self, workdir: &str) -> Option<SyncPanelState> {
        let recent_limit = self.message_window_limit_for_scroll();
        let mut recent_messages = self.db.list_sync_messages(workdir, recent_limit).ok()?;
        if recent_messages.is_empty() {
            return None;
        }

        for message in &mut recent_messages {
            if let Ok(Some(session_name)) =
                self.db.resolve_sync_actor_name(workdir, &message.agent_id)
            {
                message.agent_name = session_name;
            }
        }

        let active_agent_ids = self
            .db
            .list_active_sync_agent_ids(workdir)
            .unwrap_or_default()
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let summary = summarize_sync_context(&recent_messages, &active_agent_ids, CHATTER_LIMIT);
        let participant_count = active_agent_ids.len().max(1);

        Some(SyncPanelState {
            workdir: workdir.to_owned(),
            participant_count,
            vibe: summary.vibe,
            active_intents: summary.active_intents,
            recent_messages,
        })
    }

    fn live_session_count_for_sync(&self, workdir: &str) -> usize {
        self.db
            .list_active_sync_agent_ids(workdir)
            .map(|agent_ids| agent_ids.len())
            .unwrap_or(0)
    }

    fn message_window_limit_for_scroll(&self) -> usize {
        let steps = (self.sync_scroll_offset / MESSAGE_WINDOW_LINES_PER_STEP) as usize;
        (RECENT_MESSAGE_LIMIT + steps.saturating_mul(MESSAGE_WINDOW_ITEMS_PER_STEP))
            .min(MAX_RECENT_MESSAGE_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::models::{Agent, Cli};
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn sample_agent(id: &str, workdir: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "Track activity".to_string(),
            trigger: None,
            cli: Cli::new("opencode"),
            model: None,
            working_dir: Some(workdir.to_string()),
            enabled: true,
            created_at: Utc::now(),
            log_path: "/tmp/test.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    #[test]
    fn activity_panel_auto_shows_for_single_agent_when_messages_exist() {
        let db = test_db();
        db.insert_sync_message(
            "/tmp/project",
            "agent-a",
            "copilot",
            crate::domain::sync::MessageKind::Info,
            "first activity",
            None,
        )
        .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(sample_agent("bg-1", "/tmp/project"))];
        app.selected = 0;

        let state = app
            .activity_panel_state()
            .expect("activity panel should render");

        assert_eq!(state.workdir, "/tmp/project");
        assert_eq!(state.participant_count, 1);
        assert_eq!(state.recent_messages.len(), 1);
    }

    #[test]
    fn activity_panel_manual_hide_persists_until_reenabled() {
        let db = test_db();
        db.insert_sync_message(
            "/tmp/project",
            "agent-a",
            "copilot",
            crate::domain::sync::MessageKind::Info,
            "first activity",
            None,
        )
        .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(sample_agent("bg-1", "/tmp/project"))];
        app.selected = 0;

        assert!(app.activity_panel_state().is_some());

        app.toggle_activity_panel();
        assert!(app.activity_panel_state().is_none());

        app.toggle_activity_panel();
        assert!(app.activity_panel_state().is_some());
    }

    #[test]
    fn activity_panel_hides_intents_for_inactive_agents() {
        let db = test_db();
        let intent_payload = serde_json::to_string(&crate::domain::sync::IntentPayload {
            mission: "Old mission".to_string(),
            impact: crate::domain::sync::MissionImpact::High,
            description: "should not show when inactive".to_string(),
        })
        .expect("serialize intent payload");
        db.insert_sync_message(
            "/tmp/project",
            "agent-a",
            "copilot",
            crate::domain::sync::MessageKind::Intent,
            "intent",
            Some(intent_payload.as_str()),
        )
        .expect("insert intent");

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(sample_agent("bg-1", "/tmp/project"))];
        app.selected = 0;

        let state = app
            .activity_panel_state()
            .expect("activity panel should render");

        assert!(state.active_intents.is_empty());
    }

    #[test]
    fn activity_panel_expands_message_window_with_scroll() {
        let db = test_db();
        for index in 0..(RECENT_MESSAGE_LIMIT + 4) {
            db.insert_sync_message(
                "/tmp/project",
                &format!("agent-{index}"),
                "copilot",
                crate::domain::sync::MessageKind::Info,
                &format!("message-{index}"),
                None,
            )
            .expect("insert sync message");
        }

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(sample_agent("bg-1", "/tmp/project"))];
        app.selected = 0;

        let compact = app
            .activity_panel_state()
            .expect("activity panel should render");
        assert_eq!(compact.recent_messages.len(), RECENT_MESSAGE_LIMIT);

        app.sync_scroll_offset = MESSAGE_WINDOW_LINES_PER_STEP;
        let expanded = app
            .activity_panel_state()
            .expect("activity panel should render");
        assert!(expanded.recent_messages.len() > RECENT_MESSAGE_LIMIT);
    }
}
