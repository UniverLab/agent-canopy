//! CT1 multi-face right panel: the switching rule.
//!
//! One panel, three faces — **activity**, **knowledge**, **loop** — with a
//! strict priority: a pinned face always wins; otherwise a running loop is
//! the resting face (a STATE); knowledge insertion and backlog change are
//! EVENTS that take the panel for a 10-second dwell and then fall back.
//! A newer event replaces the current dwell and restarts it; events never
//! queue. While the user is interacting with the panel (scrolling it, or
//! with focus inside it) no automatic switch happens and the pending switch
//! is dropped, not deferred.
//!
//! The tick only ever writes `App::panel_face` and its bookkeeping — it
//! never touches `App::focus`, so an automatic switch can never steal
//! keyboard focus — and it only assigns when the computed face differs, so
//! rendering never flickers or forces a full-screen redraw.

use std::time::{Duration, Instant};

use super::types::{App, PanelFace};
use crate::domain::loops::LoopStatus;

/// How long a knowledge/backlog event holds the panel before it falls back
/// to the resting face.
pub(crate) const PANEL_DWELL_SECS: u64 = 10;

/// Picker rows: automatic first (the default), then one per face.
pub(crate) const PANEL_PICKER_OPTIONS: [Option<PanelFace>; 4] = [
    None,
    Some(PanelFace::Activity),
    Some(PanelFace::Knowledge),
    Some(PanelFace::Loop),
];

impl App {
    /// STATE input: true while any loop is running, regardless of workdir.
    pub(crate) fn panel_loop_running(&self) -> bool {
        self.loops.iter().any(|lp| lp.status == LoopStatus::Running)
    }

    /// The resting face: Loop while a loop is running, Activity otherwise.
    pub(crate) fn panel_resting_face(&self) -> PanelFace {
        if self.panel_loop_running() {
            PanelFace::Loop
        } else {
            PanelFace::Activity
        }
    }

    /// True while the user is interacting with the panel: scrolled recently,
    /// scrolled over the panel this tick, focused inside it, or working the
    /// picker. While true, automatic switches are dropped, not deferred.
    pub(crate) fn panel_is_interacting(&self) -> bool {
        self.panel_focused || self.panel_interacting || self.panel_picker_open
    }

    /// Record a knowledge/backlog event: it takes the panel for a dwell of
    /// 10 seconds. A newer event during a dwell replaces it and restarts
    /// the dwell — events never queue. While pinned or interacting the event
    /// is dropped outright.
    pub(crate) fn fire_panel_event(&mut self, face: PanelFace, reason: &str) {
        if self.panel_pinned.is_some() || self.panel_is_interacting() {
            return;
        }
        self.panel_dwell_face = Some(face);
        self.panel_dwell_until = Some(Instant::now() + Duration::from_secs(PANEL_DWELL_SECS));
        self.panel_dwell_reason = Some(reason.to_string());
    }

    fn panel_dwell_valid(&self) -> bool {
        match (self.panel_dwell_face, self.panel_dwell_until) {
            (Some(_), Some(until)) => Instant::now() < until,
            _ => false,
        }
    }

    fn clear_panel_dwell(&mut self) {
        self.panel_dwell_face = None;
        self.panel_dwell_until = None;
        self.panel_dwell_reason = None;
    }

    /// Recompute the visible face from the switching rule. Called once per
    /// refresh tick, after the data refreshes it reads have run.
    pub(crate) fn tick_panel_face(&mut self) {
        let loop_running = self.panel_loop_running();
        let knowledge_count = self.project_knowledge.len();
        let backlog_count = self.backlog_specs.len();

        if !self.panel_baselines_init {
            self.panel_last_knowledge_count = knowledge_count;
            self.panel_last_backlog_count = backlog_count;
            self.panel_last_loop_running = loop_running;
            self.panel_baselines_init = true;
            self.panel_face = self
                .panel_pinned
                .unwrap_or_else(|| self.panel_resting_face());
            self.panel_last_reason = None;
            self.panel_interacting = false;
            return;
        }

        // EVENT inputs. Knowledge insertion and backlog change both take the
        // Knowledge face; when both change on the same tick the backlog
        // change is the newer event and wins the dwell.
        if knowledge_count != self.panel_last_knowledge_count {
            self.fire_panel_event(PanelFace::Knowledge, "new knowledge");
        }
        if backlog_count != self.panel_last_backlog_count {
            self.fire_panel_event(PanelFace::Knowledge, "backlog changed");
        }
        self.panel_last_knowledge_count = knowledge_count;
        self.panel_last_backlog_count = backlog_count;
        self.panel_last_loop_running = loop_running;

        if self
            .panel_dwell_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.clear_panel_dwell();
        }

        // A pinned face always wins. While pinned, nothing switches — clear
        // any dwell so unpinning returns to a clean automatic state.
        if let Some(pinned) = self.panel_pinned {
            self.clear_panel_dwell();
            self.panel_interacting = false;
            if self.panel_face != pinned {
                self.panel_face = pinned;
            }
            self.panel_last_reason = None;
            return;
        }

        // No automatic switch while the user is interacting with the panel —
        // scrolling it, or with focus inside it. The pending switch is
        // dropped (the dwell is cleared), not deferred.
        if self.panel_is_interacting() {
            self.clear_panel_dwell();
            self.panel_interacting = false;
            return;
        }
        self.panel_interacting = false;

        // States beat events: while a loop runs, Loop is the resting face
        // and any event dwell waits underneath it. The dwell keeps counting
        // down meanwhile, so a stale event never outlives its 10 seconds.
        let (target, reason) = if loop_running {
            (PanelFace::Loop, Some("loop running".to_string()))
        } else if self.panel_dwell_valid() {
            let reason = self.panel_dwell_reason.clone();
            (
                self.panel_dwell_face.unwrap_or(PanelFace::Knowledge),
                reason,
            )
        } else {
            (PanelFace::Activity, None)
        };

        if self.panel_face != target {
            self.panel_face = target;
            self.panel_last_reason = reason;
        } else if !self.panel_dwell_valid() && !loop_running {
            // Settled on the resting face with nothing driving it: no badge.
            self.panel_last_reason = None;
        }
    }

    /// Seconds left on the current dwell, if any.
    pub(crate) fn panel_dwell_remaining_secs(&self) -> Option<u64> {
        let until = self.panel_dwell_until?;
        let remaining = until.saturating_duration_since(Instant::now());
        if self.panel_dwell_face.is_none() || remaining.is_zero() {
            return None;
        }
        Some(remaining.as_secs().max(1))
    }

    /// Badge explaining why the current face is showing, so a face never
    /// appears unexplained. `None` on the default resting face.
    pub(crate) fn panel_face_badge(&self) -> Option<String> {
        if self.panel_pinned.is_some() {
            return Some("pinned".to_string());
        }
        if let Some(reason) = self.panel_dwell_reason.clone() {
            if self.panel_dwell_valid() {
                if let Some(secs) = self.panel_dwell_remaining_secs() {
                    return Some(format!("{reason} · {secs}s"));
                }
                return Some(reason);
            }
        }
        self.panel_last_reason.clone()
    }

    /// Whether the right panel has anything to show for the current face.
    /// The Activity face keeps its existing visibility rule; Knowledge
    /// needs a selected workdir; Loop needs a loop to look at.
    pub(crate) fn panel_face_visible(&self) -> bool {
        if self.panel_pinned.is_some() {
            return true;
        }
        match self.panel_face {
            PanelFace::Activity => self.activity_panel_state().is_some(),
            PanelFace::Knowledge => {
                self.selected_activity_workdir().is_some() || self.activity_panel_state().is_some()
            }
            PanelFace::Loop => {
                self.loop_live_state.is_some()
                    || !self.loops.is_empty()
                    || self.activity_panel_state().is_some()
            }
        }
    }

    /// Pin a face from the picker (`None` = back to automatic) and persist
    /// the choice so it survives restarts. Applies immediately.
    pub(crate) fn pin_panel_face(&mut self, pin: Option<PanelFace>) {
        self.panel_pinned = pin;
        self.panel_picker_open = false;
        if let Some(face) = pin {
            self.clear_panel_dwell();
            self.panel_face = face;
            self.panel_last_reason = None;
        } else {
            self.tick_panel_face();
        }
        self.save_panel_pinned_face();
    }

    fn save_panel_pinned_face(&self) {
        let home = dirs::home_dir().unwrap_or_default();
        let canopy_dir = home.join(".canopy");
        let mut config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
        config.pinned_panel_face = self.panel_pinned.map(|face| face.label().to_string());
        let _ = config.save(&canopy_dir);
    }

    /// Read the persisted pin back into a face, ignoring unknown values.
    pub(crate) fn load_panel_pinned_face(value: &Option<String>) -> Option<PanelFace> {
        value.as_deref().and_then(PanelFace::from_str)
    }

    pub(crate) fn open_panel_picker(&mut self) {
        // The picker is automatic + every face, no more, no less.
        debug_assert_eq!(PANEL_PICKER_OPTIONS.len(), PanelFace::ALL.len() + 1);
        self.panel_picker_open = true;
        self.panel_picker_idx = PANEL_PICKER_OPTIONS
            .iter()
            .position(|option| *option == self.panel_pinned)
            .unwrap_or(0);
    }

    pub(crate) fn close_panel_picker(&mut self) {
        self.panel_picker_open = false;
    }

    pub(crate) fn move_panel_picker(&mut self, forward: bool) {
        if !self.panel_picker_open {
            return;
        }
        self.panel_picker_idx = crate::tui::selection::move_index(
            self.panel_picker_idx,
            PANEL_PICKER_OPTIONS.len(),
            forward,
        );
    }

    pub(crate) fn confirm_panel_picker(&mut self) {
        if !self.panel_picker_open {
            return;
        }
        let pin = PANEL_PICKER_OPTIONS
            .get(self.panel_picker_idx)
            .copied()
            .unwrap_or(None);
        self.pin_panel_face(pin);
    }

    /// A mouse-wheel scroll over the panel: marks the interaction and drops
    /// any pending automatic switch.
    pub(crate) fn on_panel_scrolled(&mut self) {
        self.panel_interacting = true;
        self.clear_panel_dwell();
    }

    /// A click inside the panel claims its focus; a click outside releases
    /// it. While focused, automatic switches are dropped, not deferred.
    pub(crate) fn on_panel_clicked(&mut self, inside: bool) {
        self.panel_focused = inside;
        if inside {
            self.clear_panel_dwell();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::loops::{Loop, LoopStatus};
    use crate::domain::models::{Agent, Cli};
    use crate::tui::app::types::{AgentEntry, Focus};
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn test_app() -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        // App::new already ran one tick (seeding baselines); keep the db
        // handle alive via the app itself.
        let _ = &mut app;
        app
    }

    fn sample_agent(id: &str, workdir: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "Track activity".to_string(),
            trigger: None,
            cli: Cli::new("opencode"),
            model: None,
            effort: None,
            working_dir: Some(workdir.to_string()),
            enabled: true,
            enable_at: None,
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

    fn make_loop(id: &str, status: LoopStatus) -> Loop {
        Loop {
            id: id.to_string(),
            name: format!("loop {id}"),
            description: None,
            workdir: "/tmp/project".to_string(),
            status,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            hooks: std::collections::BTreeMap::new(),
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
        }
    }

    fn expire_dwell(app: &mut App) {
        app.panel_dwell_until = Some(Instant::now() - Duration::from_secs(1));
    }

    #[test]
    fn panel_face_priority_pinned_over_state() {
        let mut app = test_app();
        app.loops = vec![make_loop("l1", LoopStatus::Running)];
        app.panel_pinned = Some(PanelFace::Knowledge);
        app.panel_face = PanelFace::Knowledge;
        app.tick_panel_face();

        assert_eq!(app.panel_face, PanelFace::Knowledge);
        assert_eq!(app.panel_face_badge().as_deref(), Some("pinned"));
    }

    #[test]
    fn panel_face_priority_state_over_event() {
        let mut app = test_app();
        app.loops = vec![make_loop("l1", LoopStatus::Running)];
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Loop);

        // A knowledge event fires while the loop runs: the loop (state)
        // keeps the panel; the event dwell is recorded underneath.
        app.fire_panel_event(PanelFace::Knowledge, "new knowledge");
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Loop);

        // When the loop ends, the pending event dwell takes the panel.
        app.loops = vec![make_loop("l1", LoopStatus::Completed)];
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Knowledge);
    }

    #[test]
    fn panel_face_dwell_10s_then_returns_to_resting() {
        let mut app = test_app();
        assert_eq!(app.panel_face, PanelFace::Activity);

        app.fire_panel_event(PanelFace::Knowledge, "new knowledge");
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Knowledge);
        let badge = app.panel_face_badge().expect("event face shows a reason");
        assert!(badge.contains("new knowledge"), "badge was {badge:?}");

        // The dwell lasts 10 seconds.
        let wait = app
            .panel_dwell_until
            .expect("dwell deadline is set")
            .saturating_duration_since(Instant::now());
        assert!(wait <= Duration::from_secs(PANEL_DWELL_SECS));
        assert!(wait > Duration::from_secs(PANEL_DWELL_SECS - 2));

        expire_dwell(&mut app);
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Activity);
        assert_eq!(app.panel_face_badge(), None);
    }

    #[test]
    fn panel_face_dwell_restart_replaces_and_restarts() {
        let mut app = test_app();
        app.fire_panel_event(PanelFace::Knowledge, "new knowledge");
        let first_until = app.panel_dwell_until.expect("first dwell set");

        std::thread::sleep(Duration::from_millis(5));
        app.fire_panel_event(PanelFace::Knowledge, "backlog changed");

        let second_until = app.panel_dwell_until.expect("second dwell set");
        assert!(
            second_until > first_until,
            "a newer event restarts the dwell"
        );
        assert_eq!(
            app.panel_dwell_reason.as_deref(),
            Some("backlog changed"),
            "a newer event replaces the old one"
        );
        assert!(app.panel_dwell_face == Some(PanelFace::Knowledge));
    }

    #[test]
    fn panel_face_drop_on_interaction_is_not_deferred() {
        let mut app = test_app();
        app.agents = vec![AgentEntry::Agent(sample_agent("bg-1", "/tmp/project"))];
        app.fire_panel_event(PanelFace::Knowledge, "new knowledge");

        // The user starts scrolling the panel: the pending switch is
        // dropped, not deferred — the panel stays put…
        app.panel_interacting = true;
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Activity);
        assert!(app.panel_dwell_face.is_none(), "dwell was dropped");

        // …and it does not fire later once the interaction ends.
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Activity);
    }

    #[test]
    fn panel_face_drop_on_focus_inside_panel() {
        let mut app = test_app();
        app.on_panel_clicked(true);
        app.fire_panel_event(PanelFace::Knowledge, "new knowledge");
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Activity);
        assert!(app.panel_dwell_face.is_none());

        // Clicking outside releases the focus; a fresh event then applies.
        app.on_panel_clicked(false);
        app.fire_panel_event(PanelFace::Knowledge, "new knowledge");
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Knowledge);
    }

    #[test]
    fn scrolling_the_main_panel_does_not_drop_panel_events() {
        let mut app = test_app();
        app.last_scroll_at = Instant::now();
        app.fire_panel_event(PanelFace::Knowledge, "new knowledge");
        app.tick_panel_face();

        assert_eq!(app.panel_face, PanelFace::Knowledge);
        assert!(app.panel_dwell_face.is_some());
    }

    #[test]
    fn panel_face_persistence_round_trips_through_config_strings() {
        for face in PanelFace::ALL {
            let stored = face.label().to_string();
            assert_eq!(
                App::load_panel_pinned_face(&Some(stored)),
                Some(face),
                "pinned face survives a config reload"
            );
        }
        assert_eq!(App::load_panel_pinned_face(&None), None);
        assert_eq!(
            App::load_panel_pinned_face(&Some("sideways".to_string())),
            None,
            "unknown values fall back to automatic mode"
        );
    }

    #[test]
    fn panel_face_no_focus_steal() {
        let mut app = test_app();
        app.focus = Focus::Agent;
        app.loops = vec![make_loop("l1", LoopStatus::Running)];
        app.tick_panel_face();

        assert_eq!(app.panel_face, PanelFace::Loop);
        assert!(matches!(app.focus, Focus::Agent));

        app.fire_panel_event(PanelFace::Knowledge, "new knowledge");
        app.loops = vec![make_loop("l1", LoopStatus::Completed)];
        app.tick_panel_face();
        assert!(matches!(app.focus, Focus::Agent));
    }

    #[test]
    fn panel_picker_pins_and_unpins() {
        let mut app = test_app();
        // Pinning through the picker never touches the real home config in
        // tests: restore automatic mode right away via the in-memory path.
        app.open_panel_picker();
        assert!(app.panel_picker_open);
        // Options are [automatic, activity, knowledge, loop]; initial pin
        // is None so the cursor starts on automatic.
        assert_eq!(app.panel_picker_idx, 0);
        app.move_panel_picker(true);
        app.move_panel_picker(true);
        assert_eq!(
            PANEL_PICKER_OPTIONS[app.panel_picker_idx],
            Some(PanelFace::Knowledge)
        );

        // Confirm would persist to ~/.canopy — exercise the selection
        // without the disk write by pinning in memory instead.
        app.close_panel_picker();
        app.panel_pinned = PANEL_PICKER_OPTIONS[app.panel_picker_idx];
        assert_eq!(app.panel_pinned, Some(PanelFace::Knowledge));
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Knowledge);

        app.panel_pinned = None;
        app.tick_panel_face();
        assert_eq!(app.panel_face, PanelFace::Activity);
    }
}
