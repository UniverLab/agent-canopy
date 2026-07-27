mod agents;
mod data;
pub mod dialog;
mod gamification;
pub(crate) mod loop_live_state;
mod project_graph;
mod sync;

use anyhow::Result;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::application::notification_service::DefaultNotificationService;
use crate::application::ports::{AgentRepository, StateRepository};
use crate::db::Database;

use super::agent::InteractiveAgent;
use super::context_transfer::{
    build_context_payload_for, initial_capture_units, interactive_capture_kind,
    interactive_line_page_count, interactive_prompt_count, ContextCaptureKind, ContextSourceKind,
    ContextTransferConfig, ContextTransferModal, ContextTransferStep,
};
use crate::domain::loops::{LoopNodeKind, LoopSpecStatus, LoopStatus};
use crate::tui::prompt_templates::PromptTemplates;

pub(crate) use crate::tui::mcp_client::send_mcp_task_run;

// ── Types ───────────────────────────────────────────────────────

pub mod session_resume;
pub mod terminal_search;
pub mod types;
pub mod utils;

pub(crate) use session_resume::build_resumed_session_args;
pub use terminal_search::TerminalSearch;
pub(crate) use types::ContextTransferSource;
pub use types::{
    AgentEntry, AgentSectionFocus, App, AutomationKind, Focus, ProjectTab, SidebarLayer,
};
use types::{LoopSidebarMeta, RagTransferModal};

impl App {
    pub fn new(db: Arc<Database>, data_dir: &Path) -> Result<Self> {
        let home = dirs::home_dir().unwrap_or_default();
        let canopy_dir = home.join(".canopy");
        let canopy_config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);

        let system_monitor_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let system_info_rx = spawn_system_monitor(&system_monitor_active);
        let mission_manager = Self::init_mission_manager(Arc::clone(&db))?;

        let mut app = Self {
            db,
            data_dir: data_dir.to_path_buf(),
            agents: Vec::new(),
            active_runs: HashMap::new(),
            recent_runs: Vec::new(),
            interactive_agents: Vec::new(),
            terminal_agents: Vec::new(),
            orphaned_sessions: Vec::new(),
            scheduled_sends_restored: false,
            split_groups: Vec::new(),
            active_split_id: None,
            split_right_focused: false,
            split_picker_open: false,
            split_picker_idx: 0,
            split_picker_orientation: crate::domain::models::SplitOrientation::Horizontal,
            split_picker_sessions: Vec::new(),
            daemon_running: false,
            daemon_pid: None,
            daemon_version: String::new(),
            selected: 0,
            focus: Focus::Home,
            sidebar_layer: SidebarLayer::Live,
            automation_kind: AutomationKind::Agent,
            project_focus: None,
            selected_project_history: 0,
            project_history_cache: HashMap::new(),
            project_preview_cache: HashMap::new(),
            log_content: String::new(),
            log_scroll: 0,
            running: true,
            new_agent_dialog: None,
            launchpad_dialog: None,
            knowledge_dialog: None,
            pending_launch_dialog: None,
            quit_confirm: false,
            delete_project_confirm: false,
            delete_loop_confirm: false,
            sidebar_brain: None,
            home_brain: None,
            sidebar_click_map: Vec::new(),
            hovered_row: None,
            sidebar_scroll_offset: 0,
            sidebar_visible_capacity: 0,
            projects: Vec::new(),
            selected_project: 0,
            agent_section_focus: AgentSectionFocus::Interactive,
            automation_loop_click_map: Vec::new(),
            project_click_map: Vec::new(),
            project_tab_click_map: Vec::new(),
            project_tab_row_click_map: Vec::new(),
            sidebar_tab_click_map: Vec::new(),
            loops: Vec::new(),
            selected_loop_id: None,
            loop_details: None,
            loop_runs: Vec::new(),
            loop_selected_spec: 0,
            loop_selected_node: 0,
            loop_editor_dialog: None,
            loop_form_dialog: None,
            loop_sidebar_meta: HashMap::new(),
            loop_live_state: None,
            loop_graph_follow: true,
            loop_graph_selected_node: None,
            backlog_specs: Vec::new(),
            selected_backlog: 0,
            global_rag_queue: Vec::new(),
            selected_rag_queue: 0,
            rag_info: crate::db::project::RagInfoSummary::default(),
            rag_file_status: Vec::new(),
            sidebar_visible: true,
            hidden_activity_workdirs: HashSet::new(),
            forced_activity_workdirs: HashSet::new(),
            term_width: 0,
            show_legend: false,
            legend_selected: 0,
            show_copied: false,
            copied_at: std::time::Instant::now() - std::time::Duration::from_secs(10),
            last_scroll_at: std::time::Instant::now() - std::time::Duration::from_secs(999),
            last_panel_inner: (0, 0),
            last_panel_x: 0,
            last_panel_y: 0,
            terminal_selection: None,
            whimsg: super::whimsg::Whimsg::new(),
            whimsg_last_log_hash: 0,
            context_transfer_modal: None,
            rag_transfer_modal: None,
            context_transfer_config: ContextTransferConfig::default(),
            prompt_templates: PromptTemplates::load_from_registry()
                .unwrap_or_else(|_| PromptTemplates::internal_templates()),
            simple_prompt_dialog: None,
            prompt_builder_sessions: HashMap::new(),
            prompt_tab_origin: None,
            prompt_raw_content_rect: None,
            notifications_enabled: true,
            notification_service: Arc::new(DefaultNotificationService),
            prev_active_run_ids: std::collections::HashSet::new(),
            animation_tick: 0,
            temperature_unit: canopy_config.temperature_unit,
            theme: crate::tui::ui::theme::Theme::resolve(&canopy_config.theme),
            suggestion_picker: None,
            terminal_histories: HashMap::new(),
            terminal_search: None,
            system_info: crate::system::SystemInfo::default(),
            system_info_target: crate::system::SystemInfo::default(),
            system_info_rx,
            system_monitor_active,
            last_system_update: std::time::Instant::now() - std::time::Duration::from_secs(10),
            last_system_frame_at: std::time::Instant::now(),
            process_start_time: std::time::Instant::now(),
            cli_usage: load_cli_usage(),
            playground_active: false,
            playground_query: String::new(),
            playground_results: Vec::new(),
            playground_selected: 0,
            playground_last_search: std::time::Instant::now(),
            playground_search_pending: false,
            playground_search_rx: None,
            playground_last_executed_query: String::new(),
            playground_detail_mode: false,
            playground_scroll: 0,
            playground_project_hash: None,
            rag_paused: false,
            rag_model_loaded: false,
            agents_rag_focused: false,
            sync_scroll_offset: 0,
            last_sync_area: None,
            workdir_system_state: HashMap::new(),
            project_relation_dialog: None,
            project_graph_edges: Vec::new(),
            project_graph_trees: Vec::new(),
            project_knowledge: Vec::new(),
            selected_knowledge: 0,
            knowledge_filter: String::new(),
            knowledge_filter_mode: false,
            nursery_path: None,
            keyboard_enhancement_active: false,
            atmosphere: crate::tui::atmosphere::SceneManager::new(),
            atmosphere_ctx: crate::tui::atmosphere::AtmosphereCtx::default(),
            atmosphere_last_mouse: (0, 0),
            atmosphere_hidden: false,
            mission_manager,
            mission_pending_events: Vec::new(),
            max_cpu_frequency_seen: None,
            uptime_anchor: None,
        };
        app.refresh()?;
        Ok(app)
    }

    /// Reload all data from the database and filesystem.
    pub fn refresh(&mut self) -> Result<()> {
        self.animation_tick = self.animation_tick.wrapping_add(1);
        self.refresh_daemon_status();
        self.refresh_agents()?;
        self.refresh_projects()?;
        self.refresh_loops()?;
        self.refresh_project_graph().ok();
        self.refresh_rag_state()?;
        self.refresh_active_runs()?;
        self.poll_interactive_agents();
        self.poll_terminal_agents();
        self.deliver_due_scheduled_sends();
        self.tick_banner_animation();
        self.ensure_sidebar_brain();
        self.refresh_log();
        self.auto_hide_sidebar();
        self.system_monitor_active
            .store(self.sidebar_visible, Ordering::Relaxed);
        self.dismiss_copied();
        self.update_whimsg_context();
        self.tick_atmosphere();
        self.tick_missions()?;
        self.resize_interactive_agents();
        self.poll_playground_search();
        self.refresh_playground_search()?;
        if let Some(dialog) = self.simple_prompt_dialog.as_mut() {
            dialog.tick_at_picker();
        }

        // Non-blocking check for updated system info from background thread
        while let Ok(info) = self.system_info_rx.try_recv() {
            self.system_info_target = info;
            self.last_system_update = std::time::Instant::now();
        }
        self.interpolate_system_info();

        Ok(())
    }

    fn interpolate_system_info(&mut self) {
        let now = std::time::Instant::now();
        let elapsed = now.saturating_duration_since(self.last_system_frame_at);
        self.last_system_frame_at = now;

        // Blend toward the latest sampled snapshot with a longer window so
        // values keep moving smoothly between monitoring samples.
        let blend = (elapsed.as_secs_f32() / 0.9).clamp(0.0, 1.0);
        if blend <= 0.0 {
            return;
        }

        blend_system_info(&mut self.system_info, &self.system_info_target, blend);
    }

    /// Perform debounced RAG search in playground mode. The search itself
    /// (embedding-model load + embed + vector search) runs on a worker
    /// thread (B23) — this only spawns it; [`Self::poll_playground_search`]
    /// applies the results when they arrive, so the UI never blocks.
    fn refresh_playground_search(&mut self) -> Result<()> {
        const PLAYGROUND_SEARCH_DEBOUNCE_MS: u128 = 2_000;

        if !self.playground_active {
            return Ok(());
        }
        if !self.playground_search_pending {
            return Ok(());
        }
        // One search at a time: results of the in-flight one arrive first,
        // and pending stays true, so a newer query re-triggers right after.
        if self.playground_search_rx.is_some() {
            return Ok(());
        }

        let since_last = self.playground_last_search.elapsed().as_millis();
        if since_last < PLAYGROUND_SEARCH_DEBOUNCE_MS {
            return Ok(());
        }

        let query = self.playground_query.trim().to_string();
        if query.is_empty() {
            self.playground_results.clear();
            self.playground_selected = 0;
            self.playground_last_executed_query.clear();
            self.playground_search_pending = false;
            return Ok(());
        }

        if self.playground_last_executed_query == query {
            self.playground_search_pending = false;
            return Ok(());
        }

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = playground_vector_search(&query, 50);
            let _ = tx.send((query, outcome));
        });
        self.playground_search_rx = Some(rx);
        Ok(())
    }

    /// Apply a finished background playground search, if any (B23). Called
    /// from the tick loop; never blocks.
    fn poll_playground_search(&mut self) {
        let Some(rx) = &self.playground_search_rx else {
            return;
        };
        let (executed_query, outcome) = match rx.try_recv() {
            Ok(message) => message,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.playground_search_rx = None;
                self.playground_search_pending = false;
                return;
            }
        };
        self.playground_search_rx = None;

        // Playground closed (Esc) while the search ran: abandon the result.
        if !self.playground_active {
            self.playground_search_pending = false;
            return;
        }

        if let Ok(results) = outcome {
            let month_ago = chrono::Utc::now().timestamp() - 30 * 24 * 3600;
            for result in &results {
                if result.distance.is_some_and(|d| d < 0.2) {
                    self.queue_mission_event(crate::tui::gamification::MissionEvent::DeepRagSearch);
                }
                if result.created_at < month_ago {
                    self.queue_mission_event(
                        crate::tui::gamification::MissionEvent::DigitalArcheologistFind,
                    );
                }
            }
            self.playground_results = results;
            self.playground_selected = 0;
        }
        self.playground_last_executed_query = executed_query;
        // Leave pending=true if the user kept typing (query changed while
        // the search ran) so the debounce re-triggers with the newer query.
        self.playground_search_pending =
            self.playground_query.trim() != self.playground_last_executed_query;
        self.playground_last_search = std::time::Instant::now();
    }

    // ── Navigation ──────────────────────────────────────────────
    //
    // The sidebar is one flat vertical ring for arrow-key purposes: pinned
    // RAG (top) → Live → Automation → Knowledge (bottom), wrapping around.
    // Inside Knowledge, once a project is entered (`project_focus.is_some()`)
    // arrows instead navigate the active tab's list exclusively — they never
    // change tabs (functional requirement 4).

    pub fn select_next(&mut self) {
        if self.agents_rag_focused {
            self.leave_rag_focus(true);
            self.reset_log_scroll();
            return;
        }
        if self.project_focus.is_some() {
            self.navigate_project_tab_list(true);
            self.reset_log_scroll();
            return;
        }
        match self.sidebar_layer {
            SidebarLayer::Live => self.navigate_live(true),
            SidebarLayer::Automation => self.navigate_automation(true),
            SidebarLayer::Knowledge => self.navigate_projects_next(),
        }
        self.reset_log_scroll();
    }

    pub fn select_prev(&mut self) {
        if self.agents_rag_focused {
            self.leave_rag_focus(false);
            self.reset_log_scroll();
            return;
        }
        if self.project_focus.is_some() {
            self.navigate_project_tab_list(false);
            self.reset_log_scroll();
            return;
        }
        match self.sidebar_layer {
            SidebarLayer::Live => self.navigate_live(false),
            SidebarLayer::Automation => self.navigate_automation(false),
            SidebarLayer::Knowledge => self.navigate_projects_prev(),
        }
        self.reset_log_scroll();
    }

    /// Indices into `app.agents` that render inside the `Live` layer
    /// (interactive sessions, terminals, orphaned sessions, split groups —
    /// everything with a PTY right now), in rendering order.
    fn live_indices(&self) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                matches!(
                    a,
                    AgentEntry::Interactive(_)
                        | AgentEntry::Terminal(_)
                        | AgentEntry::Orphaned(_)
                        | AgentEntry::Group(_)
                )
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Indices into `app.agents` that render inside the `Automation` layer's
    /// agent sub-list (background agents, including corrupt rows).
    fn automation_agent_indices(&self) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, AgentEntry::Agent(_) | AgentEntry::Corrupt(_)))
            .map(|(i, _)| i)
            .collect()
    }

    fn navigate_live(&mut self, forward: bool) {
        let indices = self.live_indices();
        if indices.is_empty() {
            self.cross_layer(SidebarLayer::Live, forward);
            return;
        }
        let current = indices.iter().position(|&i| i == self.selected);
        let next_pos = match current {
            Some(pos) if forward && pos + 1 < indices.len() => Some(pos + 1),
            Some(pos) if !forward && pos > 0 => Some(pos - 1),
            Some(_) => None,
            None => Some(0),
        };
        match next_pos {
            Some(pos) => {
                let prev = self.selected;
                self.selected = indices[pos];
                self.update_agent_section_focus_on_change(prev);
            }
            None => self.cross_layer(SidebarLayer::Live, forward),
        }
    }

    /// Automation is one flat ring for arrow-key purposes, agents rendered
    /// above loops: `[agent, agent, …, loop, loop, …]`. Running off either
    /// true end crosses to the next/previous layer (`cross_layer`) instead
    /// of bouncing between the two sub-lists.
    fn navigate_automation(&mut self, forward: bool) {
        let agent_indices = self.automation_agent_indices();
        let loop_ids: Vec<String> = self
            .active_loops()
            .into_iter()
            .map(|lp| lp.id.clone())
            .collect();
        let total = agent_indices.len() + loop_ids.len();
        if total == 0 {
            self.cross_layer(SidebarLayer::Automation, forward);
            return;
        }

        let current = match self.automation_kind {
            AutomationKind::Agent => agent_indices.iter().position(|&i| i == self.selected),
            AutomationKind::Loop => self
                .selected_loop_id
                .as_ref()
                .and_then(|id| loop_ids.iter().position(|v| v == id))
                .map(|pos| agent_indices.len() + pos),
        };
        let next_pos = match current {
            Some(pos) if forward && pos + 1 < total => Some(pos + 1),
            Some(pos) if !forward && pos > 0 => Some(pos - 1),
            Some(_) => None,
            None => Some(if forward { 0 } else { total - 1 }),
        };
        let Some(pos) = next_pos else {
            self.cross_layer(SidebarLayer::Automation, forward);
            return;
        };

        if pos < agent_indices.len() {
            self.automation_kind = AutomationKind::Agent;
            let prev = self.selected;
            self.selected = agent_indices[pos];
            self.update_agent_section_focus_on_change(prev);
        } else {
            self.automation_kind = AutomationKind::Loop;
            self.selected_loop_id = Some(loop_ids[pos - agent_indices.len()].clone());
            self.refresh_loops_selection();
        }
    }

    fn navigate_projects_next(&mut self) {
        if self.projects.is_empty() {
            self.cross_layer(SidebarLayer::Knowledge, true);
            return;
        }
        let next = self.selected_project + 1;
        if next < self.projects.len() {
            self.selected_project = next;
            self.refresh_loops_selection();
            return;
        }
        self.cross_layer(SidebarLayer::Knowledge, true);
    }

    fn navigate_projects_prev(&mut self) {
        if self.projects.is_empty() {
            self.cross_layer(SidebarLayer::Knowledge, false);
            return;
        }
        if self.selected_project > 0 {
            self.selected_project -= 1;
            self.refresh_loops_selection();
            return;
        }
        self.cross_layer(SidebarLayer::Knowledge, false);
    }

    /// Ran off the end of `from`'s list: move to the next/previous layer in
    /// ring order (RAG → Live → Automation → Knowledge → RAG…), skipping a
    /// layer if it has nothing to select, and landing on the RAG pinned
    /// summary when it has activity.
    fn cross_layer(&mut self, from: SidebarLayer, forward: bool) {
        let ring = [
            SidebarLayer::Live,
            SidebarLayer::Automation,
            SidebarLayer::Knowledge,
        ];
        let start = ring.iter().position(|&l| l == from).unwrap_or(0);
        let len = ring.len();
        for step in 1..=len {
            let idx = if forward {
                (start + step) % len
            } else {
                (start + len - step) % len
            };
            if self.enter_layer(ring[idx], forward) {
                return;
            }
        }
        // Nothing navigable anywhere else — try RAG, else stay put.
        if self.rag_info.has_rag_activity() {
            self.enter_rag_focus();
        }
    }

    /// Try focusing the first/last navigable item of `layer`. Returns
    /// `false` (and touches nothing) if `layer` has nothing to select, so
    /// `cross_layer` can keep looking.
    fn enter_layer(&mut self, layer: SidebarLayer, forward: bool) -> bool {
        match layer {
            SidebarLayer::Live => {
                let indices = self.live_indices();
                if indices.is_empty() {
                    return false;
                }
                self.sidebar_layer = SidebarLayer::Live;
                let prev = self.selected;
                self.selected = if forward {
                    indices[0]
                } else {
                    *indices.last().unwrap()
                };
                self.update_agent_section_focus_on_change(prev);
                true
            }
            SidebarLayer::Automation => {
                let agent_indices = self.automation_agent_indices();
                let loop_ids: Vec<String> = self
                    .active_loops()
                    .into_iter()
                    .map(|lp| lp.id.clone())
                    .collect();
                if agent_indices.is_empty() && loop_ids.is_empty() {
                    return false;
                }
                self.sidebar_layer = SidebarLayer::Automation;
                // Agents render above loops, so entering forward (from
                // above) lands on agents first; entering backward (from
                // below) lands on loops first — whichever list is empty is
                // skipped.
                if forward {
                    if !agent_indices.is_empty() {
                        self.automation_kind = AutomationKind::Agent;
                        let prev = self.selected;
                        self.selected = agent_indices[0];
                        self.update_agent_section_focus_on_change(prev);
                    } else {
                        self.automation_kind = AutomationKind::Loop;
                        self.selected_loop_id = Some(loop_ids[0].clone());
                        self.refresh_loops_selection();
                    }
                } else if !loop_ids.is_empty() {
                    self.automation_kind = AutomationKind::Loop;
                    self.selected_loop_id = Some(loop_ids.last().unwrap().clone());
                    self.refresh_loops_selection();
                } else {
                    self.automation_kind = AutomationKind::Agent;
                    let prev = self.selected;
                    self.selected = *agent_indices.last().unwrap();
                    self.update_agent_section_focus_on_change(prev);
                }
                true
            }
            SidebarLayer::Knowledge => {
                if self.projects.is_empty() {
                    return false;
                }
                self.sidebar_layer = SidebarLayer::Knowledge;
                self.selected_project = if forward { 0 } else { self.projects.len() - 1 };
                self.refresh_loops_selection();
                true
            }
        }
    }

    fn enter_rag_focus(&mut self) {
        self.agents_rag_focused = true;
    }

    /// Leaving the pinned RAG summary: land on the layer nearest it (`Live`
    /// going forward, `Knowledge` wrapping around going backward), else stay
    /// on RAG if nothing else is navigable.
    fn leave_rag_focus(&mut self, forward: bool) {
        self.agents_rag_focused = false;
        let start = if forward {
            SidebarLayer::Automation
        } else {
            SidebarLayer::Knowledge
        };
        // Try the immediate neighbor first (Live going forward, Knowledge
        // going backward), then fall back through the ring.
        if forward && self.enter_layer(SidebarLayer::Live, true) {
            return;
        }
        if !forward && self.enter_layer(SidebarLayer::Knowledge, false) {
            return;
        }
        self.cross_layer(start, forward);
    }

    /// Move a project's active `ProjectTab` list selection. Arrows never
    /// change tabs (functional requirement 4) — only the list inside the
    /// current tab moves, or wraps within it.
    fn navigate_project_tab_list(&mut self, forward: bool) {
        match self.project_focus {
            Some(ProjectTab::Overview) | None => {}
            Some(ProjectTab::Backlog) => {
                if self.backlog_specs.is_empty() {
                    return;
                }
                self.selected_backlog = if forward {
                    (self.selected_backlog + 1) % self.backlog_specs.len()
                } else {
                    self.selected_backlog
                        .checked_sub(1)
                        .unwrap_or(self.backlog_specs.len() - 1)
                };
            }
            Some(ProjectTab::Knowledge) => {
                if forward {
                    self.navigate_knowledge_next();
                } else {
                    self.navigate_knowledge_prev();
                }
            }
            Some(ProjectTab::History) => {
                let len = self.selected_project_history_entries().len();
                if len == 0 {
                    return;
                }
                self.selected_project_history = if forward {
                    (self.selected_project_history + 1) % len
                } else {
                    self.selected_project_history
                        .checked_sub(1)
                        .unwrap_or(len - 1)
                };
            }
        }
    }

    fn navigate_knowledge_next(&mut self) {
        let filtered = self.filtered_knowledge_indices();
        if filtered.is_empty() {
            return;
        }
        let current = filtered
            .iter()
            .position(|&idx| idx == self.selected_knowledge)
            .unwrap_or(0);
        self.selected_knowledge = filtered[(current + 1) % filtered.len()];
    }

    fn navigate_knowledge_prev(&mut self) {
        let filtered = self.filtered_knowledge_indices();
        if filtered.is_empty() {
            return;
        }
        let current = filtered
            .iter()
            .position(|&idx| idx == self.selected_knowledge)
            .unwrap_or(0);
        let next = current.checked_sub(1).unwrap_or(filtered.len() - 1);
        self.selected_knowledge = filtered[next];
    }

    pub(crate) fn update_agent_section_focus_on_change(&mut self, _prev_selected: usize) {
        if let Some(agent) = self.agents.get(self.selected) {
            match agent {
                AgentEntry::Agent(_) | AgentEntry::Corrupt(_) => {
                    self.sidebar_layer = SidebarLayer::Automation;
                    self.automation_kind = AutomationKind::Agent;
                }
                AgentEntry::Interactive(_) | AgentEntry::Orphaned(_) => {
                    self.sidebar_layer = SidebarLayer::Live;
                    self.agent_section_focus = AgentSectionFocus::Interactive;
                }
                AgentEntry::Terminal(_) => {
                    self.sidebar_layer = SidebarLayer::Live;
                    self.agent_section_focus = AgentSectionFocus::Terminal;
                }
                AgentEntry::Group(_) => {
                    self.sidebar_layer = SidebarLayer::Live;
                    self.agent_section_focus = AgentSectionFocus::Groups;
                }
            };
        }
    }

    fn reset_log_scroll(&mut self) {
        self.log_scroll = 0;
        self.sidebar_scroll_offset = 0;
    }

    /// Select an agent by its flat index into `self.agents`, mirroring the
    /// bookkeeping arrow-key navigation performs (section focus, RAG focus,
    /// scroll reset). Used by sidebar mouse clicks.
    pub(crate) fn select_agent_at(&mut self, idx: usize) {
        if idx >= self.agents.len() {
            return;
        }
        let prev = self.selected;
        self.selected = idx;
        self.agents_rag_focused = false;
        self.update_agent_section_focus_on_change(prev);
        self.reset_log_scroll();
    }

    pub fn scroll_log_down(&mut self) {
        self.log_scroll = self.log_scroll.saturating_add(3);
    }

    pub fn scroll_log_up(&mut self) {
        self.log_scroll = self.log_scroll.saturating_sub(3);
    }

    fn refresh_projects(&mut self) -> Result<()> {
        self.projects = self.db.list_projects()?;
        if self.projects.is_empty() {
            self.selected_project = 0;
        } else {
            self.selected_project = self.selected_project.min(self.projects.len() - 1);
        }
        self.refresh_project_knowledge()?;
        self.refresh_backlog_specs()?;
        self.refresh_project_preview_cache();
        // Keep an already-open History tab live instead of only loading it
        // once on first show — cheap (one indexed query) and scoped to just
        // the project currently being looked at.
        if self.project_focus == Some(ProjectTab::History) {
            if let Some(hash) = self.selected_project().map(|p| p.hash.clone()) {
                self.load_project_history(&hash);
            }
        }
        Ok(())
    }

    /// Recompute the Knowledge layer's per-project Preview summary cache
    /// (pending backlog count, knowledge entry count, last activity, loop
    /// badge). Cheap aggregate queries over the small `projects` list, run
    /// once per refresh tick — never per keystroke/highlight move
    /// (functional requirement 3).
    fn refresh_project_preview_cache(&mut self) {
        let running_workdirs: HashSet<String> = self
            .active_loops()
            .iter()
            .filter(|lp| lp.status == LoopStatus::Running)
            .map(|lp| lp.workdir.clone())
            .collect();

        let mut cache = HashMap::new();
        for project in &self.projects {
            let pending_backlog = self
                .db
                .list_specs(Some(project.path.as_str()), None, true)
                .map(|specs| specs.len())
                .unwrap_or(0);
            let knowledge_entries = self
                .db
                .list_project_knowledge(&project.hash, None, 200)
                .map(|nodes| nodes.len())
                .unwrap_or(0);
            let last_activity = self
                .db
                .list_loops(Some(project.path.as_str()))
                .ok()
                .and_then(|loops| loops.iter().map(|lp| lp.created_at.timestamp()).max());
            cache.insert(
                project.hash.clone(),
                types::ProjectPreviewSummary {
                    pending_backlog,
                    knowledge_entries,
                    last_activity,
                    loop_running: running_workdirs.contains(&project.path),
                },
            );
        }
        self.project_preview_cache = cache;
    }

    pub(crate) fn selected_project_preview(&self) -> Option<&types::ProjectPreviewSummary> {
        let project = self.selected_project()?;
        self.project_preview_cache.get(&project.hash)
    }

    /// Load (or reload) the persisted History tab entries for the project
    /// with `hash` into the cache.
    fn load_project_history(&mut self, hash: &str) {
        let Some(workdir) = self
            .projects
            .iter()
            .find(|p| p.hash == hash)
            .map(|p| p.path.clone())
        else {
            return;
        };
        let entries = self
            .db
            .list_project_history(&workdir, 100)
            .unwrap_or_default();
        self.project_history_cache.insert(hash.to_string(), entries);
    }

    /// Persisted History entries for the currently selected project, lazily
    /// loading them into the cache on first access.
    pub(crate) fn selected_project_history_entries(
        &mut self,
    ) -> &[crate::db::project::ProjectHistoryEntry] {
        let Some(hash) = self.selected_project().map(|p| p.hash.clone()) else {
            return &[];
        };
        if !self.project_history_cache.contains_key(&hash) {
            self.load_project_history(&hash);
        }
        self.project_history_cache
            .get(&hash)
            .map_or(&[], |v| v.as_slice())
    }

    /// Reload the standalone/backlog specs shown in the sidebar's `Backlog`
    /// section, tag-filtered to the selected project's workdir (or
    /// unfiltered when no project is registered/selected). Runs on the same
    /// cadence as `refresh_projects` — no dedicated polling loop.
    fn refresh_backlog_specs(&mut self) -> Result<()> {
        let workdir_filter = self.selected_project().map(|p| p.path.clone());
        self.backlog_specs = self.db.list_specs(workdir_filter.as_deref(), None, true)?;
        if self.backlog_specs.is_empty() {
            self.selected_backlog = 0;
        } else {
            self.selected_backlog = self.selected_backlog.min(self.backlog_specs.len() - 1);
        }
        Ok(())
    }

    pub fn refresh_project_knowledge(&mut self) -> Result<()> {
        if let Some(project) = self.projects.get(self.selected_project) {
            self.project_knowledge = self.db.list_project_knowledge(&project.hash, None, 50)?;
            self.normalize_selected_knowledge();
        } else {
            self.project_knowledge.clear();
            self.selected_knowledge = 0;
        }
        Ok(())
    }

    fn refresh_loops(&mut self) -> Result<()> {
        self.loops = self.db.list_loops(None)?;
        self.refresh_loop_sidebar_meta();
        self.refresh_loops_selection();
        Ok(())
    }

    /// Recompute the sidebar's per-loop spec progress ("done/total") and
    /// blocked status (a `Paused` loop whose latest run recorded a
    /// `loop_report_blocker` description). One `list_loop_specs` +, for
    /// paused loops, one `list_loop_runs_for_loop` query per loop — bounded
    /// by the (typically small) number of loops, run on the existing
    /// refresh cadence rather than a dedicated poller.
    fn refresh_loop_sidebar_meta(&mut self) {
        let mut meta = HashMap::new();
        for lp in &self.loops {
            let specs = self.db.list_loop_specs(&lp.id).unwrap_or_default();
            let total = specs.len();
            let done = specs
                .iter()
                .filter(|spec| spec.status == LoopSpecStatus::Completed)
                .count();
            let blocked = lp.status == LoopStatus::Paused
                && self
                    .db
                    .list_loop_runs_for_loop(&lp.id)
                    .ok()
                    .and_then(|runs| runs.last().and_then(|run| run.output.clone()))
                    .is_some_and(|output| output.get("blocker").is_some());
            meta.insert(
                lp.id.clone(),
                LoopSidebarMeta {
                    done,
                    total,
                    blocked,
                },
            );
        }
        self.loop_sidebar_meta = meta;
    }

    /// Non-terminal loops (`Draft`/`Running`/`Paused`) for the sidebar's
    /// `Loops` section, with running loops sorted first, then paused
    /// (including blocked), then draft — ties broken by the existing
    /// `created_at DESC` order from `list_loops`.
    pub fn active_loops(&self) -> Vec<&crate::domain::loops::Loop> {
        let mut loops: Vec<&crate::domain::loops::Loop> = self
            .loops
            .iter()
            .filter(|lp| !matches!(lp.status, LoopStatus::Completed | LoopStatus::Failed))
            .collect();
        loops.sort_by_key(|lp| match lp.status {
            LoopStatus::Running => 0,
            LoopStatus::Paused => 1,
            LoopStatus::Draft => 2,
            LoopStatus::Completed | LoopStatus::Failed => 3,
        });
        loops
    }

    pub(crate) fn refresh_loops_selection(&mut self) {
        let visible = self.visible_loops();
        if visible.is_empty() {
            self.selected_loop_id = None;
            self.loop_details = None;
            self.loop_runs.clear();
            self.loop_selected_spec = 0;
            self.loop_selected_node = 0;
            self.loop_live_state = None;
            self.loop_graph_follow = true;
            self.loop_graph_selected_node = None;
            return;
        }

        let previous_selected = self.selected_loop_id.clone();
        if self
            .selected_loop_id
            .as_ref()
            .is_none_or(|selected| !visible.iter().any(|lp| lp.id == *selected))
        {
            self.selected_loop_id = Some(visible[0].id.clone());
        }

        let selected_changed = previous_selected != self.selected_loop_id;
        let Some(selected_id) = self.selected_loop_id.clone() else {
            return;
        };
        self.loop_details = self.db.get_loop_details(&selected_id).ok().flatten();
        if selected_changed {
            self.loop_selected_spec = self.default_loop_spec_index();
            self.loop_selected_node = 0;
            self.loop_graph_follow = true;
            self.loop_graph_selected_node = None;
        } else {
            self.clamp_loop_selection();
        }
        self.refresh_loop_runs_for_selected_spec();
        self.select_default_loop_node_if_needed(selected_changed);
        self.refresh_loop_live_state();
    }

    /// Assemble a fresh [`LoopLiveState`] snapshot for the currently selected
    /// loop. Zero cost when no loop is selected (no queries).
    fn refresh_loop_live_state(&mut self) {
        self.loop_live_state = self
            .loop_details
            .as_ref()
            .and_then(|details| loop_live_state::assemble_loop_live_state(&self.db, details));

        // A manually-highlighted node that no longer exists in the
        // (possibly just-advanced) effective graph falls back to
        // auto-follow rather than pointing at a stale/missing node.
        if !self.loop_graph_follow {
            let still_present = self.loop_live_state.as_ref().is_some_and(|state| {
                self.loop_graph_selected_node
                    .as_deref()
                    .is_some_and(|id| state.effective_nodes.iter().any(|n| n.id == id))
            });
            if !still_present {
                self.loop_graph_follow = true;
                self.loop_graph_selected_node = None;
            }
        }
    }

    /// Move the live loop view's graph highlight to the next/previous node
    /// (by `position` order) in the current spec's effective graph, entering
    /// manual-inspection mode. No-op when there's no live state or graph.
    pub fn loop_graph_move_highlight(&mut self, forward: bool) {
        let ids: Vec<String> = match self.loop_live_state.as_ref() {
            Some(state) if !state.effective_nodes.is_empty() => {
                state.effective_nodes.iter().map(|n| n.id.clone()).collect()
            }
            _ => return,
        };

        let current = self.loop_graph_highlighted_node_id().map(str::to_string);
        let idx = current
            .as_deref()
            .and_then(|id| ids.iter().position(|n| n == id))
            .unwrap_or(0);
        let next_idx = if forward {
            (idx + 1) % ids.len()
        } else {
            idx.checked_sub(1).unwrap_or(ids.len() - 1)
        };
        self.loop_graph_selected_node = Some(ids[next_idx].clone());
        self.loop_graph_follow = false;
    }

    /// Return the live loop view to auto-follow, discarding any manual
    /// node-inspection selection.
    pub fn loop_graph_reset_follow(&mut self) {
        self.loop_graph_follow = true;
        self.loop_graph_selected_node = None;
    }

    /// The node id currently highlighted in the live loop view: the
    /// engine's current node while auto-following, else the manually
    /// selected node.
    pub fn loop_graph_highlighted_node_id(&self) -> Option<&str> {
        if self.loop_graph_follow {
            self.loop_live_state.as_ref()?.current_node_id.as_deref()
        } else {
            self.loop_graph_selected_node.as_deref()
        }
    }

    /// Run info (status/started_at/iteration/output tail) for the live loop
    /// view's currently highlighted node — reuses the snapshot's own
    /// current-node fields when the highlight matches it (no query), else
    /// looks up the manually-highlighted node directly.
    pub fn loop_graph_highlighted_node_run_info(&self) -> loop_live_state::NodeRunInfo {
        let Some(state) = self.loop_live_state.as_ref() else {
            return loop_live_state::NodeRunInfo::default();
        };
        let highlighted = self.loop_graph_highlighted_node_id();
        if highlighted == state.current_node_id.as_deref() {
            return loop_live_state::NodeRunInfo {
                status: state.current_node_status,
                started_at: state.current_node_started_at,
                iteration: state.current_node_iteration,
                output_tail: state.current_node_output_tail.clone(),
            };
        }
        let (Some(spec_id), Some(node_id)) = (state.current_spec_id.as_deref(), highlighted) else {
            return loop_live_state::NodeRunInfo::default();
        };
        self.loop_node_run_info(spec_id, node_id)
    }

    fn refresh_rag_state(&mut self) -> Result<()> {
        self.global_rag_queue = self.db.list_rag_queue(50)?;
        self.rag_paused = self
            .db
            .get_state("rag_paused")?
            .map(|v| v == "1")
            .unwrap_or(false);
        self.rag_model_loaded = crate::rag::status::is_model_loaded(&self.db);

        let (queued, processing) = self
            .db
            .rag_queue_counts()
            .unwrap_or((self.rag_info.queued_items, self.rag_info.processing_items));
        let total_chunks = self
            .db
            .get_state("rag_total_chunks")?
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(self.rag_info.total_chunks);
        let indexed_files = self
            .db
            .get_state("rag_indexed_files")?
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(self.rag_info.indexed_files);
        self.rag_info = crate::db::project::RagInfoSummary {
            total_chunks,
            indexed_files,
            queued_items: queued,
            processing_items: processing,
        };

        if self.global_rag_queue.is_empty() {
            self.selected_rag_queue = 0;
        } else {
            self.selected_rag_queue = self
                .selected_rag_queue
                .min(self.global_rag_queue.len().saturating_sub(1));
        }

        if !self.rag_info.has_rag_activity() {
            self.agents_rag_focused = false;
        }

        self.rag_file_status = self.db.rag_per_file_status().unwrap_or_default();

        Ok(())
    }

    pub fn selected_agent(&self) -> Option<&AgentEntry> {
        self.agents.get(self.selected)
    }

    pub fn selected_project(&self) -> Option<&crate::domain::project::Project> {
        self.projects.get(self.selected_project)
    }

    pub fn visible_loops(&self) -> Vec<&crate::domain::loops::Loop> {
        self.loops.iter().collect()
    }

    pub fn selected_loop(&self) -> Option<&crate::domain::loops::Loop> {
        let selected_id = self.selected_loop_id.as_ref()?;
        self.loops.iter().find(|lp| lp.id == *selected_id)
    }

    pub fn selected_loop_spec(&self) -> Option<&crate::domain::loops::LoopSpecDetails> {
        self.loop_details
            .as_ref()
            .and_then(|details| details.specs.get(self.loop_selected_spec))
    }

    pub fn selected_loop_node(&self) -> Option<&crate::domain::loops::LoopNode> {
        self.selected_loop_spec()
            .and_then(|spec| spec.nodes.get(self.loop_selected_node))
    }

    /// Latest run info (status/started_at/iteration/output tail) for
    /// `node_id` within `spec_id` — for a node the user has navigated to in
    /// the graph, which may differ from `loop_live_state`'s auto-detected
    /// current node.
    pub(crate) fn loop_node_run_info(
        &self,
        spec_id: &str,
        node_id: &str,
    ) -> loop_live_state::NodeRunInfo {
        loop_live_state::resolve_node_run_info(&self.db, spec_id, node_id)
    }

    pub fn delete_selected_project(&mut self) -> Result<()> {
        let Some(hash) = self.selected_project().map(|p| p.hash.clone()) else {
            return Ok(());
        };
        self.db.delete_project(&hash)?;
        self.refresh_projects()?;
        self.refresh_loops()?;
        self.refresh_project_graph().ok();
        self.refresh_rag_state()?;
        Ok(())
    }

    pub fn delete_selected_loop(&mut self) -> Result<()> {
        let Some(lp) = self.selected_loop() else {
            return Ok(());
        };
        self.db.delete_loop(&lp.id)?;
        self.refresh_loops()?;
        self.refresh_projects()?;
        self.refresh_rag_state()?;
        Ok(())
    }

    pub fn delete_selected_knowledge(&mut self) -> Result<()> {
        let Some(node) = self.project_knowledge.get(self.selected_knowledge) else {
            return Ok(());
        };
        let id = node.id.clone();
        self.db.delete_intelligence_node(&id)?;
        self.refresh_project_knowledge()?;
        Ok(())
    }

    pub fn filtered_knowledge_indices(&self) -> Vec<usize> {
        let query = self.knowledge_filter.trim().to_lowercase();
        self.project_knowledge
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                if query.is_empty() {
                    return true;
                }

                node.title.to_lowercase().contains(&query)
                    || node.body.to_lowercase().contains(&query)
                    || node.kind.to_lowercase().contains(&query)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    pub fn append_knowledge_filter(&mut self, value: char) {
        self.knowledge_filter.push(value);
        self.normalize_selected_knowledge();
    }

    pub fn pop_knowledge_filter(&mut self) {
        self.knowledge_filter.pop();
        self.normalize_selected_knowledge();
    }

    pub fn clear_knowledge_filter(&mut self) {
        self.knowledge_filter.clear();
        self.normalize_selected_knowledge();
    }

    pub fn enter_knowledge_filter_mode(&mut self) {
        self.knowledge_filter_mode = true;
    }

    pub fn exit_knowledge_filter_mode(&mut self) {
        self.knowledge_filter_mode = false;
    }

    fn normalize_selected_knowledge(&mut self) {
        let filtered = self.filtered_knowledge_indices();
        if filtered.is_empty() {
            self.selected_knowledge = 0;
            return;
        }

        if filtered.contains(&self.selected_knowledge) {
            return;
        }

        self.selected_knowledge = filtered[0];
    }

    /// Entry point for arrow-down/up from the `Home` screen: focus the
    /// nearest navigable edge of the sidebar ring (RAG → Live → Automation →
    /// Knowledge), mirroring `cross_layer`'s ring order.
    pub(crate) fn focus_sidebar_from_edge(&mut self, from_top: bool) {
        if from_top {
            if self.rag_info.has_rag_activity() {
                self.enter_rag_focus();
                return;
            }
            for layer in [
                SidebarLayer::Live,
                SidebarLayer::Automation,
                SidebarLayer::Knowledge,
            ] {
                if self.enter_layer(layer, true) {
                    return;
                }
            }
        } else {
            for layer in [
                SidebarLayer::Knowledge,
                SidebarLayer::Automation,
                SidebarLayer::Live,
            ] {
                if self.enter_layer(layer, false) {
                    return;
                }
            }
            if self.rag_info.has_rag_activity() {
                self.enter_rag_focus();
            }
        }
    }

    /// Jump directly to the next sidebar tab (F2 / right-click), skipping
    /// tabs with nothing to select — a keyboard-only shortcut alongside
    /// arrow-key ring navigation.
    pub(crate) fn cycle_sidebar_layer(&mut self) {
        self.agents_rag_focused = false;
        let ring = Self::SIDEBAR_TAB_RING;
        let start_idx = Self::sidebar_tab_index(self.sidebar_layer);
        for step in 1..=ring.len() {
            let layer = ring[(start_idx + step) % ring.len()];
            if self.enter_layer(layer, true) {
                return;
            }
        }
    }

    /// Left-to-right order of the sidebar tab strip. Mirrors `SIDEBAR_TABS`
    /// in the renderer, which is what Shift+←/→ has to agree with for the
    /// arrows to move the way the strip looks.
    const SIDEBAR_TAB_RING: [SidebarLayer; 3] = [
        SidebarLayer::Live,
        SidebarLayer::Automation,
        SidebarLayer::Knowledge,
    ];

    fn sidebar_tab_index(layer: SidebarLayer) -> usize {
        Self::SIDEBAR_TAB_RING
            .iter()
            .position(|&l| l == layer)
            .unwrap_or(0)
    }

    /// Shift+←/→ — move exactly one tab in `forward`'s direction, wrapping
    /// at the ends. Deliberately does NOT skip empty tabs the way F2 does:
    /// a directional key that silently jumps two cells because the one in
    /// between was empty reads as a bug, and the empty tab's own state is
    /// worth seeing.
    pub(crate) fn step_sidebar_tab(&mut self, forward: bool) {
        let ring = Self::SIDEBAR_TAB_RING;
        let idx = Self::sidebar_tab_index(self.sidebar_layer);
        let next = if forward {
            (idx + 1) % ring.len()
        } else {
            idx.checked_sub(1).unwrap_or(ring.len() - 1)
        };
        self.switch_sidebar_tab(ring[next]);
    }

    /// Enter a highlighted project's Focus tab bar (functional requirement
    /// 4). `tab` defaults to `Overview`; History lazily loads on first show.
    pub(crate) fn enter_project_focus(&mut self, tab: ProjectTab) {
        self.project_focus = Some(tab);
        if tab == ProjectTab::History {
            let _ = self.selected_project_history_entries();
        }
    }

    /// Leave a project's Focus tab bar back to the sidebar (Esc).
    pub(crate) fn exit_project_focus(&mut self) {
        self.project_focus = None;
    }

    /// Tab/Shift+Tab or `]`/`[` inside a project's Focus tab bar.
    pub(crate) fn cycle_project_tab(&mut self, forward: bool) {
        let Some(current) = self.project_focus else {
            return;
        };
        let idx = ProjectTab::ALL
            .iter()
            .position(|&t| t == current)
            .unwrap_or(0);
        let len = ProjectTab::ALL.len();
        let next = if forward {
            (idx + 1) % len
        } else {
            idx.checked_sub(1).unwrap_or(len - 1)
        };
        self.enter_project_focus(ProjectTab::ALL[next]);
    }

    /// Direct hotkey (o/b/k/h) to jump straight to a tab.
    pub(crate) fn open_project_tab(&mut self, tab: ProjectTab) {
        self.enter_project_focus(tab);
    }

    /// Mouse click on the active tab's list at display row `idx` — sets the
    /// tab's selection directly (unlike arrow keys, which move by one).
    pub(crate) fn set_project_tab_row(&mut self, idx: usize) {
        match self.project_focus {
            Some(ProjectTab::Backlog) => {
                if idx < self.backlog_specs.len() {
                    self.selected_backlog = idx;
                }
            }
            Some(ProjectTab::Knowledge) => {
                if let Some(&node_idx) = self.filtered_knowledge_indices().get(idx) {
                    self.selected_knowledge = node_idx;
                }
            }
            Some(ProjectTab::History) => {
                if idx < self.selected_project_history_entries().len() {
                    self.selected_project_history = idx;
                }
            }
            Some(ProjectTab::Overview) | None => {}
        }
    }

    /// Mouse click on a sidebar tab label — switches straight to it,
    /// entering its first item when it has one (consistent with
    /// `cycle_sidebar_layer`'s keyboard behavior). Unlike the keyboard
    /// cycle, this also switches into a tab with nothing to select, since a
    /// deliberate click on a visible tab must always land there — a click on
    /// an empty Automation should show its empty state, not silently no-op.
    pub(crate) fn switch_sidebar_tab(&mut self, layer: SidebarLayer) {
        self.agents_rag_focused = false;
        if !self.enter_layer(layer, true) {
            self.sidebar_layer = layer;
        }
    }

    pub fn activate_playground(&mut self) {
        self.playground_active = true;
        self.reset_playground_state();
        // Personal RAG is global — no project_hash filter.
        self.playground_project_hash = None;
    }

    pub fn deactivate_playground(&mut self) {
        self.playground_active = false;
        self.reset_playground_state();
    }

    fn reset_playground_state(&mut self) {
        self.playground_query.clear();
        self.playground_results.clear();
        self.playground_selected = 0;
        self.playground_search_pending = false;
        self.playground_last_executed_query.clear();
        self.playground_detail_mode = false;
        self.playground_scroll = 0;
    }

    pub fn toggle_rag_pause(&mut self) {
        let new_val = !self.rag_paused;
        let _ = self
            .db
            .set_state("rag_paused", if new_val { "1" } else { "0" });
        self.rag_paused = new_val;
    }

    pub fn cycle_loop_spec(&mut self, forward: bool) {
        let Some(details) = self.loop_details.as_ref() else {
            return;
        };
        if details.specs.is_empty() {
            return;
        }

        self.loop_selected_spec = if forward {
            (self.loop_selected_spec + 1) % details.specs.len()
        } else {
            self.loop_selected_spec
                .checked_sub(1)
                .unwrap_or(details.specs.len() - 1)
        };
        self.loop_selected_node = 0;
        self.refresh_loop_runs_for_selected_spec();
        self.select_default_loop_node_if_needed(true);
        self.reset_log_scroll();
    }

    pub fn open_loop_editor_dialog(&mut self) -> Result<()> {
        let Some(node) = self.selected_loop_node() else {
            return Ok(());
        };

        let (title, help, buffer, mode) = self.build_editor_dialog_content(node);

        self.loop_editor_dialog = Some(crate::tui::app::types::LoopEditorDialog::new(
            node.id.clone(),
            node.name.clone(),
            title,
            help,
            buffer,
            mode,
        ));
        self.focus = Focus::LoopEditorDialog;
        Ok(())
    }

    /// U10: duplicate the highlighted loop node — a fresh, unwired copy of its
    /// config into the same graph — then open the editor on the copy so its
    /// prompt/config can be tweaked (the closest thing the TUI has to a
    /// creation flow to pre-fill). Ensemble member/join nodes are
    /// engine-managed, so duplicating a whole ensemble is left to the
    /// `loop_copy_ensemble` MCP tool and this is a no-op for those.
    pub fn duplicate_selected_loop_node(&mut self) -> Result<()> {
        let Some(node) = self.selected_loop_node() else {
            return Ok(());
        };
        let node = node.clone();

        // Ensemble-owned (member or join) nodes can't be copied as plain
        // nodes — that would break the "no nested ensembles" invariant.
        if node.kind == LoopNodeKind::Join
            || self.db.get_ensemble_by_member_node(&node.id)?.is_some()
            || self.db.get_ensemble_by_join_node(&node.id)?.is_some()
        {
            return Ok(());
        }

        let siblings = match (&node.spec_id, &node.loop_id) {
            (Some(spec_id), _) => self.db.list_loop_nodes(spec_id)?,
            (None, Some(loop_id)) => self.db.list_loop_nodes_for_loop(loop_id)?,
            (None, None) => return Ok(()),
        };
        let next_position = siblings.last().map(|n| n.position + 1).unwrap_or(1);

        let copy = crate::domain::loops::LoopNode {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: node.spec_id.clone(),
            loop_id: node.loop_id.clone(),
            name: format!("{} (copy)", node.name),
            kind: node.kind,
            config: node.config,
            position: next_position,
            created_at: chrono::Utc::now(),
        };
        self.db.insert_loop_node(&copy)?;
        self.refresh_loops()?;

        // Pre-fill the editor with the copy's config (identical to the
        // source's) so the user can immediately adjust it.
        let (title, help, buffer, mode) = self.build_editor_dialog_content(&copy);
        self.loop_editor_dialog = Some(crate::tui::app::types::LoopEditorDialog::new(
            copy.id.clone(),
            copy.name,
            title,
            help,
            buffer,
            mode,
        ));
        self.focus = Focus::LoopEditorDialog;
        Ok(())
    }

    fn build_editor_dialog_content(
        &self,
        node: &crate::domain::loops::LoopNode,
    ) -> (
        String,
        String,
        String,
        crate::tui::app::types::LoopEditorMode,
    ) {
        if node.kind == LoopNodeKind::Agent {
            self.build_agent_prompt_dialog(node)
        } else {
            self.build_node_config_dialog(node)
        }
    }

    fn build_agent_prompt_dialog(
        &self,
        node: &crate::domain::loops::LoopNode,
    ) -> (
        String,
        String,
        String,
        crate::tui::app::types::LoopEditorMode,
    ) {
        let prompt = node
            .config
            .get("prompt_template")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        (
            format!(" Loop Prompt · {} ", node.name),
            "Ctrl+S save  ·  Enter newline  ·  Esc cancel".to_string(),
            prompt.to_string(),
            crate::tui::app::types::LoopEditorMode::AgentPrompt,
        )
    }

    fn build_node_config_dialog(
        &self,
        node: &crate::domain::loops::LoopNode,
    ) -> (
        String,
        String,
        String,
        crate::tui::app::types::LoopEditorMode,
    ) {
        (
            format!(" Loop Config · {} ", node.name),
            "Ctrl+S save JSON  ·  Enter newline  ·  Esc cancel".to_string(),
            serde_json::to_string_pretty(&node.config).unwrap_or_default(),
            crate::tui::app::types::LoopEditorMode::NodeConfig,
        )
    }

    pub fn cancel_loop_editor_dialog(&mut self) {
        self.loop_editor_dialog = None;
        self.focus = Focus::Preview;
    }

    pub fn save_loop_editor_dialog(&mut self) -> Result<()> {
        let Some(dialog) = self.loop_editor_dialog.take() else {
            return Ok(());
        };
        let Some(node) = self.db.get_loop_node(&dialog.node_id)? else {
            self.focus = Focus::Preview;
            return Ok(());
        };

        let updated_config = self.compute_updated_node_config(&dialog, &node)?;
        self.db.update_loop_node_details(
            &dialog.node_id,
            None,
            None,
            Some(&updated_config),
            None,
        )?;
        self.focus = Focus::Preview;
        self.refresh_loops()?;
        Ok(())
    }

    fn compute_updated_node_config(
        &mut self,
        dialog: &crate::tui::app::types::LoopEditorDialog,
        node: &crate::domain::loops::LoopNode,
    ) -> Result<serde_json::Value> {
        match dialog.mode {
            crate::tui::app::types::LoopEditorMode::AgentPrompt => {
                Ok(Self::update_prompt_config(&node.config, &dialog.buffer))
            }
            crate::tui::app::types::LoopEditorMode::NodeConfig => {
                match serde_json::from_str::<serde_json::Value>(&dialog.buffer) {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        let mut d = dialog.clone();
                        d.parse_error = Some(format!("JSON error: {e}"));
                        self.loop_editor_dialog = Some(d);
                        self.focus = Focus::LoopEditorDialog;
                        Err(anyhow::anyhow!("Invalid JSON"))
                    }
                }
            }
        }
    }

    fn update_prompt_config(config: &serde_json::Value, prompt: &str) -> serde_json::Value {
        if let Some(object) = config.as_object() {
            let mut updated = object.clone();
            updated.insert(
                "prompt_template".to_string(),
                serde_json::Value::String(prompt.to_string()),
            );
            serde_json::Value::Object(updated)
        } else {
            serde_json::json!({ "prompt_template": prompt })
        }
    }

    pub fn selected_playground_chunk(&self) -> Option<&crate::rag::vector_store::SearchResult> {
        self.playground_results.get(self.playground_selected)
    }

    pub fn toggle_activity_panel(&mut self) {
        if self.sidebar_layer == SidebarLayer::Knowledge {
            return;
        }

        let Some(workdir) = self.selected_activity_workdir().map(str::to_owned) else {
            return;
        };
        let panel_rendered = self
            .activity_panel_layout_width(self.term_width, self.activity_panel_state().is_some())
            > 0;

        if self.hidden_activity_workdirs.remove(&workdir) {
            self.forced_activity_workdirs.insert(workdir);
            self.sync_scroll_offset = 0;
            return;
        }

        // Nueva lógica: siempre permite togglear el panel de actividad con F3,
        // aunque no haya actividad previa en el workdir seleccionado.
        if panel_rendered || self.forced_activity_workdirs.contains(&workdir) {
            self.forced_activity_workdirs.remove(&workdir);
            self.hidden_activity_workdirs.insert(workdir);
        } else {
            self.forced_activity_workdirs.insert(workdir);
        }
        self.sync_scroll_offset = 0;
    }

    fn live_agent_for_entry(&self, entry: &AgentEntry) -> Option<&InteractiveAgent> {
        match entry {
            AgentEntry::Interactive(idx) => self.interactive_agents.get(*idx),
            AgentEntry::Terminal(idx) => self.terminal_agents.get(*idx),
            AgentEntry::Agent(_)
            | AgentEntry::Corrupt(_)
            | AgentEntry::Group(_)
            | AgentEntry::Orphaned(_) => None,
        }
    }

    fn selected_live_agent(&self) -> Option<&InteractiveAgent> {
        self.selected_agent()
            .and_then(|entry| self.live_agent_for_entry(entry))
    }

    /// Return the working directory of the currently selected agent,
    /// or the parent of the data directory as a fallback.
    pub fn current_workdir(&self) -> PathBuf {
        if let Some(workdir) = self.workdir_for_projects_mode() {
            return workdir;
        }
        if let Some(workdir) = self.workdir_for_selected_agent() {
            return workdir;
        }
        self.data_dir
            .parent()
            .unwrap_or(&self.data_dir)
            .to_path_buf()
    }

    fn workdir_for_projects_mode(&self) -> Option<PathBuf> {
        if self.sidebar_layer != SidebarLayer::Knowledge {
            return None;
        }
        self.selected_project().map(|p| PathBuf::from(&p.path))
    }

    fn workdir_for_selected_agent(&self) -> Option<PathBuf> {
        self.selected_live_agent()
            .map(|agent| PathBuf::from(&agent.working_dir))
    }

    /// Return a unique key for the current prompt-builder session.
    /// Uses the agent/session ID when available, falls back to workdir path.
    pub fn current_prompt_session_key(&self) -> String {
        if let Some(key) = self.prompt_session_key_for_selected_agent() {
            return key;
        }
        if let Some(key) = self.prompt_session_key_for_selected_project() {
            return key;
        }
        format!("workdir:{}", self.current_workdir().display())
    }

    fn prompt_session_key_for_selected_agent(&self) -> Option<String> {
        let entry = self.selected_agent()?;
        match entry {
            types::AgentEntry::Interactive(idx) => {
                let agent = self.interactive_agents.get(*idx)?;
                Some(format!("interactive:{}", agent.id))
            }
            types::AgentEntry::Terminal(idx) => {
                let agent = self.terminal_agents.get(*idx)?;
                Some(format!("terminal:{}", agent.id))
            }
            types::AgentEntry::Agent(a) => Some(format!("agent:{}", a.id)),
            types::AgentEntry::Corrupt(_)
            | types::AgentEntry::Group(_)
            | types::AgentEntry::Orphaned(_) => None,
        }
    }

    fn prompt_session_key_for_selected_project(&self) -> Option<String> {
        if self.sidebar_layer != SidebarLayer::Knowledge {
            return None;
        }
        let project = self.selected_project()?;
        Some(format!("project:{}", project.path))
    }

    pub fn focused_agent_name(&self) -> String {
        self.selected_live_agent()
            .map(|agent| agent.name.clone())
            .unwrap_or_default()
    }

    pub fn selected_id(&self) -> String {
        self.selected_agent()
            .map(|a| a.id(self).to_string())
            .unwrap_or_else(|| "—".to_string())
    }

    /// Record a CLI launch in usage stats and persist to disk.
    pub fn record_cli_usage(&mut self, cli_name: &str) {
        self.cli_usage.record(cli_name);
        let _ =
            dirs::home_dir().and_then(|h| self.cli_usage.save(&h.join(".canopy")).ok().map(|_| ()));
    }

    pub fn toggle_enable(&self) -> Result<()> {
        let Some(AgentEntry::Agent(agent)) = self.agents.get(self.selected) else {
            return Ok(());
        };

        self.db.update_agent_enabled(&agent.id, !agent.enabled)?;
        Ok(())
    }

    fn auto_hide_sidebar(&mut self) {
        let Ok((tw, _th)) = ratatui::crossterm::terminal::size() else {
            return;
        };

        self.term_width = tw;
        let should_hide =
            self.focus == Focus::Agent && self.selected_live_agent().is_some() && tw < 80;
        let should_show = tw >= 80 && !self.sidebar_visible;
        if should_hide {
            self.sidebar_visible = false;
        } else if should_show {
            self.sidebar_visible = true;
        }
    }

    fn clamp_loop_selection(&mut self) {
        let Some(details) = self.loop_details.as_ref() else {
            self.loop_selected_spec = 0;
            self.loop_selected_node = 0;
            return;
        };
        if details.specs.is_empty() {
            self.loop_selected_spec = 0;
            self.loop_selected_node = 0;
            return;
        }

        self.loop_selected_spec = self.loop_selected_spec.min(details.specs.len() - 1);
        let node_count = details.specs[self.loop_selected_spec].nodes.len();
        self.loop_selected_node = if node_count == 0 {
            0
        } else {
            self.loop_selected_node.min(node_count - 1)
        };
    }

    fn default_loop_spec_index(&self) -> usize {
        self.loop_details
            .as_ref()
            .and_then(|details| {
                details
                    .specs
                    .iter()
                    .position(|spec| spec.spec.status == LoopSpecStatus::Running)
                    .or_else(|| {
                        details
                            .specs
                            .iter()
                            .position(|spec| spec.spec.status == LoopSpecStatus::Pending)
                    })
            })
            .unwrap_or(0)
    }

    fn refresh_loop_runs_for_selected_spec(&mut self) {
        self.loop_runs.clear();
        let Some(spec) = self.selected_loop_spec() else {
            return;
        };
        self.loop_runs = self
            .db
            .list_loop_runs_for_spec(&spec.spec.id)
            .unwrap_or_default();
    }

    fn select_default_loop_node_if_needed(&mut self, reset: bool) {
        let Some(spec) = self.selected_loop_spec() else {
            self.loop_selected_node = 0;
            return;
        };
        if spec.nodes.is_empty() {
            self.loop_selected_node = 0;
            return;
        }

        if !reset && self.loop_selected_node < spec.nodes.len() {
            return;
        }

        let current_node_id = self
            .loop_runs
            .iter()
            .rev()
            .find(|run| run.status == crate::domain::loops::LoopRunStatus::Running)
            .or_else(|| self.loop_runs.last())
            .map(|run| run.node_id.as_str());

        self.loop_selected_node = current_node_id
            .and_then(|node_id| spec.nodes.iter().position(|node| node.id == node_id))
            .unwrap_or(0);
    }

    fn update_whimsg_context(&mut self) {
        use crate::tui::whimsg::WhimContext;

        if !self.daemon_running {
            self.whimsg.set_ambient(WhimContext::AgentFailed);
            self.whimsg.notify_event(WhimContext::AgentFailed);
            return;
        }

        self.check_recent_run_events();

        if self.last_scroll_at.elapsed() < std::time::Duration::from_secs(5) {
            self.whimsg.set_ambient(WhimContext::Scrolling);
            return;
        }

        self.check_log_context();
        self.update_ambient_context();
    }

    fn check_recent_run_events(&mut self) {
        use crate::tui::whimsg::WhimContext;
        let now = Utc::now();
        let statuses: Vec<_> = self
            .recent_runs
            .iter()
            .filter_map(|run| {
                let finished = run.finished_at?;
                if (now - finished).num_seconds() >= 60 {
                    return None;
                }
                match run.status {
                    crate::domain::models::RunStatus::Error
                    | crate::domain::models::RunStatus::Timeout => Some(WhimContext::AgentFailed),
                    crate::domain::models::RunStatus::Success => Some(WhimContext::AgentDone),
                    _ => None,
                }
            })
            .collect();
        for ctx in statuses {
            self.whimsg.notify_event(ctx);
        }
    }

    fn check_log_context(&mut self) {
        let raw_log = self.selected_log_excerpt();
        if raw_log.is_empty() {
            return;
        }

        self.notify_whimsg_for_log(&raw_log);
    }

    fn selected_log_excerpt(&self) -> String {
        self.selected_live_agent()
            .map(|agent| agent.visible_text())
            .unwrap_or_else(|| self.log_content.clone())
    }

    fn notify_whimsg_for_log(&mut self, raw_log: &str) {
        use crate::tui::whimsg::WhimContext;

        let log_hash = calculate_log_hash(raw_log);
        if log_hash == self.whimsg_last_log_hash {
            return;
        }
        self.whimsg_last_log_hash = log_hash;

        let log_up = raw_log.to_uppercase();
        if log_contains_error(&log_up) {
            self.whimsg.notify_event(WhimContext::AgentFailed);
        } else if log_contains_success(&log_up) {
            self.whimsg.notify_event(WhimContext::AgentDone);
        } else if log_contains_spawn(&log_up) {
            self.whimsg.notify_event(WhimContext::AgentSpawned);
        }
    }

    fn update_ambient_context(&mut self) {
        use crate::tui::whimsg::WhimContext;
        let running = self
            .interactive_agents
            .iter()
            .filter(|a| a.status == crate::tui::agent::AgentStatus::Running)
            .count();
        let has_active_runs = !self.active_runs.is_empty();

        let ctx = if running >= 3 || (running >= 1 && has_active_runs) {
            WhimContext::Busy
        } else if has_active_runs {
            WhimContext::TaskRunning
        } else {
            WhimContext::Idle
        };
        self.whimsg.set_ambient(ctx);
    }

    // ── Split Groups ────────────────────────────────────────────

    /// Open the split picker to pair the current session with another.
    pub fn open_split_picker(&mut self) {
        let sessions = self.available_split_sessions();
        if sessions.len() < 2 {
            return;
        }

        self.split_picker_sessions = sessions;
        self.split_picker_idx = 0;
        self.split_picker_orientation = crate::domain::models::SplitOrientation::Horizontal;
        self.split_picker_open = true;
    }

    fn available_split_sessions(&self) -> Vec<(String, String)> {
        self.interactive_agents
            .iter()
            .map(|agent| (agent.name.clone(), "Interactive".to_string()))
            .chain(
                self.terminal_agents
                    .iter()
                    .map(|agent| (agent.name.clone(), "Terminal".to_string())),
            )
            .collect()
    }

    fn selected_session_name(&self) -> Option<String> {
        self.selected_live_agent().map(|agent| agent.name.clone())
    }

    /// Create a split group from the current session and the picker selection.
    pub fn create_split(&mut self) {
        let Some(current_name) = self.selected_session_name() else {
            return;
        };
        let Some((other_name, _)) = self
            .split_picker_sessions
            .get(self.split_picker_idx)
            .cloned()
        else {
            return;
        };
        if current_name == other_name {
            return;
        }

        let id = format!("split-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let group = crate::domain::models::SplitGroup {
            id: id.clone(),
            orientation: self.split_picker_orientation,
            session_a: current_name,
            session_b: other_name,
            created_at: Utc::now(),
        };
        let _ = self.db.insert_group(
            &group.id,
            group.orientation.as_str(),
            &group.session_a,
            &group.session_b,
        );
        self.active_split_id = Some(id);
        self.split_groups.push(group);
        self.split_picker_open = false;
        self.split_right_focused = false;
        self.focus = Focus::Agent;
    }

    /// Dissolve the currently active split group.
    pub fn dissolve_split(&mut self) {
        if let Some(id) = self.active_split_id.take() {
            let _ = self.db.delete_group(&id);
            self.split_groups.retain(|g| g.id != id);
        }
        self.split_picker_open = false;
    }

    // ── Context Transfer ────────────────────────────────────────

    /// Open the context transfer modal for the currently focused interactive or terminal agent.
    pub fn open_context_transfer_modal(&mut self) {
        let source = self.selected_context_transfer_source();
        self.open_context_transfer_from_source(source);
    }

    /// Open context transfer for the focused split panel's session.
    pub fn open_context_transfer_for_split(&mut self) {
        let source = self
            .active_split_session_name()
            .and_then(|name| self.context_transfer_source_by_name(&name));
        self.open_context_transfer_from_source(source);
    }

    /// Close the modal and return focus to the agent.
    pub fn close_context_transfer_modal(&mut self) {
        self.context_transfer_modal = None;
        self.focus = Focus::Agent;
    }

    /// Advance the modal from Preview to AgentPicker.
    pub fn context_transfer_to_picker(&mut self) {
        let Some(modal) = &mut self.context_transfer_modal else {
            return;
        };
        if modal.step != ContextTransferStep::Preview {
            return;
        }

        modal.step = ContextTransferStep::AgentPicker;
        modal.picker_selected = 0;
    }

    fn interactive_picker_destination(&self, dest_entry_idx: usize) -> Option<usize> {
        self.picker_interactive_entries()
            .get(dest_entry_idx)
            .copied()
            .filter(|idx| *idx < self.interactive_agents.len())
    }

    fn focus_interactive_agent(&mut self, dest_ia_idx: usize) {
        if let Some(entry_pos) = self.find_agent_entry_position(dest_ia_idx) {
            self.selected = entry_pos;
        }
        self.focus = Focus::Agent;
    }

    fn find_agent_entry_position(&self, dest_ia_idx: usize) -> Option<usize> {
        self.agents
            .iter()
            .position(|entry| matches!(entry, AgentEntry::Interactive(idx) if *idx == dest_ia_idx))
    }

    fn open_context_prompt_dialog(&mut self, context_payload: String, rag_query: Option<String>) {
        let mut initial_content = HashMap::from([("context".to_string(), context_payload)]);
        if let Some(query) = rag_query.filter(|query| !query.trim().is_empty()) {
            initial_content.insert("rag_search".to_string(), format!("global: {query}"));
        }
        self.open_simple_prompt_dialog(Some(initial_content));
    }

    fn context_transfer_source_for_entry(
        &self,
        entry: &AgentEntry,
    ) -> Option<ContextTransferSource> {
        match entry {
            AgentEntry::Interactive(idx) => self
                .interactive_agents
                .get(*idx)
                .map(|_| ContextTransferSource::Interactive(*idx)),
            AgentEntry::Terminal(idx) => self
                .terminal_agents
                .get(*idx)
                .map(|_| ContextTransferSource::Terminal(*idx)),
            AgentEntry::Agent(_)
            | AgentEntry::Corrupt(_)
            | AgentEntry::Group(_)
            | AgentEntry::Orphaned(_) => None,
        }
    }

    fn context_transfer_source_for_kind(
        &self,
        kind: ContextSourceKind,
        idx: usize,
    ) -> Option<ContextTransferSource> {
        match kind {
            ContextSourceKind::Interactive => self
                .interactive_agents
                .get(idx)
                .map(|_| ContextTransferSource::Interactive(idx)),
            ContextSourceKind::Terminal => self
                .terminal_agents
                .get(idx)
                .map(|_| ContextTransferSource::Terminal(idx)),
        }
    }

    fn context_transfer_agent(&self, source: ContextTransferSource) -> Option<&InteractiveAgent> {
        match source {
            ContextTransferSource::Interactive(idx) => self.interactive_agents.get(idx),
            ContextTransferSource::Terminal(idx) => self.terminal_agents.get(idx),
        }
    }

    fn context_transfer_source_kind(source: ContextTransferSource) -> ContextSourceKind {
        match source {
            ContextTransferSource::Interactive(_) => ContextSourceKind::Interactive,
            ContextTransferSource::Terminal(_) => ContextSourceKind::Terminal,
        }
    }

    fn interactive_capture_units(
        agent: &InteractiveAgent,
        capture_kind: ContextCaptureKind,
    ) -> usize {
        match capture_kind {
            ContextCaptureKind::Prompts => interactive_prompt_count(agent),
            ContextCaptureKind::LinePages => interactive_line_page_count(agent),
        }
    }

    fn context_transfer_max_units_for_source(
        &self,
        source: ContextTransferSource,
        capture_kind: ContextCaptureKind,
    ) -> Option<usize> {
        let ContextTransferSource::Interactive(_) = source else {
            return Some(20);
        };
        let agent = self.context_transfer_agent(source)?;
        Some(Self::interactive_capture_units(agent, capture_kind).max(1))
    }

    /// Execute the context transfer to the selected destination agent.
    ///
    /// 1. Builds the payload.
    /// 2. Switches focus to destination.
    /// 3. Opens Prompt Template dialog with payload pre-filled in the "context" section.
    pub fn execute_context_transfer(&mut self, dest_entry_idx: usize) {
        let Some(modal) = self.context_transfer_modal.take() else {
            return;
        };
        let Some(dest_ia_idx) = self.interactive_picker_destination(dest_entry_idx) else {
            return;
        };
        let Some(payload) = self.build_context_transfer_payload(&modal) else {
            return;
        };

        self.focus_interactive_agent(dest_ia_idx);
        self.open_context_prompt_dialog(payload, None);
    }

    pub(crate) fn refresh_context_transfer_preview(&mut self) {
        let Some((source, n_units, capture_kind)) =
            self.context_transfer_modal.as_ref().and_then(|modal| {
                self.modal_source(modal)
                    .map(|source| (source, modal.n_units, modal.capture_kind))
            })
        else {
            return;
        };

        let Some(preview) =
            self.build_context_transfer_payload_from_source(source, n_units, capture_kind)
        else {
            return;
        };

        if let Some(modal) = self.context_transfer_modal.as_mut() {
            modal.payload_preview = preview;
        }
    }

    pub(crate) fn context_transfer_max_units(&self) -> Option<usize> {
        let modal = self.context_transfer_modal.as_ref()?;
        self.context_transfer_max_units_for_source(self.modal_source(modal)?, modal.capture_kind)
    }

    fn selected_context_transfer_source(&self) -> Option<ContextTransferSource> {
        self.selected_agent()
            .and_then(|entry| self.context_transfer_source_for_entry(entry))
    }

    fn active_split_session_name(&self) -> Option<String> {
        let split_id = self.active_split_id.as_ref()?;
        let group = self
            .split_groups
            .iter()
            .find(|group| group.id == *split_id)?;
        Some(if self.split_right_focused {
            group.session_b.clone()
        } else {
            group.session_a.clone()
        })
    }

    fn context_transfer_source_by_name(&self, name: &str) -> Option<ContextTransferSource> {
        if let Some(idx) = self
            .interactive_agents
            .iter()
            .position(|agent| agent.name == name)
        {
            return Some(ContextTransferSource::Interactive(idx));
        }
        self.terminal_agents
            .iter()
            .position(|agent| agent.name == name)
            .map(ContextTransferSource::Terminal)
    }

    fn open_context_transfer_from_source(&mut self, source: Option<ContextTransferSource>) {
        let Some(source) = source else {
            return;
        };
        let Some(mut modal) = self.modal_for_context_transfer_source(source) else {
            return;
        };

        if let Some(preview) = self.build_context_transfer_payload(&modal) {
            modal.payload_preview = preview;
        }

        self.context_transfer_modal = Some(modal);
        self.focus = Focus::ContextTransfer;
    }

    fn modal_for_context_transfer_source(
        &self,
        source: ContextTransferSource,
    ) -> Option<ContextTransferModal> {
        match source {
            ContextTransferSource::Interactive(idx) => self.build_interactive_transfer_modal(idx),
            ContextTransferSource::Terminal(idx) => self.build_terminal_transfer_modal(idx),
        }
    }

    fn build_interactive_transfer_modal(&self, idx: usize) -> Option<ContextTransferModal> {
        let agent = self.context_transfer_agent(ContextTransferSource::Interactive(idx))?;
        let capture_kind = interactive_capture_kind(agent);
        let max_units = Self::interactive_capture_units(agent, capture_kind);
        let initial_units = if capture_kind == ContextCaptureKind::LinePages {
            1
        } else {
            initial_capture_units(max_units, &self.context_transfer_config)
        };
        Some(ContextTransferModal::new(idx, capture_kind, initial_units))
    }

    fn build_terminal_transfer_modal(&self, idx: usize) -> Option<ContextTransferModal> {
        self.context_transfer_agent(ContextTransferSource::Terminal(idx))?;
        Some(ContextTransferModal::new_terminal(idx, 1))
    }

    fn modal_source(&self, modal: &ContextTransferModal) -> Option<ContextTransferSource> {
        self.context_transfer_source_for_kind(modal.source_kind(), modal.source_agent_idx)
    }

    fn build_context_transfer_payload(&self, modal: &ContextTransferModal) -> Option<String> {
        self.build_context_transfer_payload_from_source(
            self.modal_source(modal)?,
            modal.n_units,
            modal.capture_kind,
        )
    }

    fn build_context_transfer_payload_from_source(
        &self,
        source: ContextTransferSource,
        n_units: usize,
        capture_kind: ContextCaptureKind,
    ) -> Option<String> {
        let agent = self.context_transfer_agent(source)?;
        Some(build_context_payload_for(
            agent,
            n_units,
            Self::context_transfer_source_kind(source),
            capture_kind,
        ))
    }

    /// Collect interactive agent indices for use in the picker list.
    pub fn picker_interactive_entries(&self) -> Vec<usize> {
        (0..self.interactive_agents.len()).collect()
    }

    pub fn open_rag_transfer_modal(&mut self) {
        let Some(chunk) = self.selected_playground_chunk() else {
            return;
        };

        let query = self.playground_query.trim().to_string();
        let context_payload = format!(
            "kind: rag_chunk\nquery: {}\npath: {}\ndistance: {}\ncontent:\n{}",
            query,
            chunk.file_path,
            chunk
                .distance
                .map_or("—".to_string(), |d| format!("{d:.4}")),
            chunk.content
        );

        self.rag_transfer_modal = Some(RagTransferModal {
            picker_selected: 0,
            query,
            context_payload,
        });
        self.focus = Focus::RagTransfer;
    }

    pub fn close_rag_transfer_modal(&mut self) {
        self.rag_transfer_modal = None;
        self.focus = Focus::Preview;
    }

    pub fn execute_rag_transfer(&mut self, dest_entry_idx: usize) {
        let Some(modal) = self.rag_transfer_modal.take() else {
            return;
        };
        let Some(dest_ia_idx) = self.interactive_picker_destination(dest_entry_idx) else {
            return;
        };

        self.focus_interactive_agent(dest_ia_idx);
        self.open_context_prompt_dialog(modal.context_payload, Some(modal.query));
    }

    fn session_panel_size() -> (u16, u16) {
        let (tw, th) = ratatui::crossterm::terminal::size().unwrap_or((120, 40));
        (tw.saturating_sub(28), th.saturating_sub(4))
    }

    fn interactive_agent_names(&self) -> Vec<&str> {
        self.interactive_agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect()
    }

    fn terminal_agent_names(&self) -> Vec<&str> {
        self.terminal_agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect()
    }

    fn resume_session_accent(
        cli_config: Option<&crate::domain::cli_config::CliConfig>,
    ) -> ratatui::style::Color {
        cli_config
            .and_then(|config| config.accent_color)
            .map(|[r, g, b]| ratatui::style::Color::Rgb(r, g, b))
            .unwrap_or(ratatui::style::Color::Rgb(102, 187, 106))
    }

    fn resume_interactive_session(
        &mut self,
        session: &crate::db::session::InteractiveSession,
        canopy_config: &crate::domain::canopy_config::CanopyConfig,
        cols: u16,
        rows: u16,
        current_boot_id: Option<&str>,
    ) {
        let cli = crate::domain::models::Cli::from_str(&session.cli);
        let cli_config = canopy_config.get_cli(cli.as_str());
        let resume_args = build_resumed_session_args(
            session,
            cli_config.and_then(|config| config.interactive_args.as_deref()),
            cli_config.and_then(|config| config.resume_args.as_deref()),
            cli_config.and_then(|config| config.session_resume_cmd.as_deref()),
            cli_config.and_then(|config| config.yolo_flag.as_deref()),
        );
        let existing_ids = self.interactive_agent_names();

        let mut used_args = resume_args.clone();
        let agent = match InteractiveAgent::spawn(
            cli.clone(),
            &session.working_dir,
            cols,
            rows,
            resume_args.as_deref(),
            cli_config.and_then(|config| config.fallback_interactive_args.as_deref()),
            Self::resume_session_accent(cli_config),
            Some(&session.name),
            &existing_ids,
            None,
            cli_config.and_then(|config| config.model_flag.as_deref()),
            None,
        ) {
            Ok(agent) => agent,
            Err(e) => {
                // The old CLI session lock may still be held by a dead-but-not-reaped
                // process, or the resume flags themselves may be stale. Fall back to a
                // plain fresh session rather than leaving the user with nothing.
                tracing::warn!(
                    "Failed to auto-resume session '{}': {e}; retrying as a fresh session",
                    session.name
                );
                let fresh_args = cli_config
                    .and_then(|config| config.interactive_args.as_deref())
                    .map(str::to_string);
                match InteractiveAgent::spawn(
                    cli.clone(),
                    &session.working_dir,
                    cols,
                    rows,
                    fresh_args.as_deref(),
                    cli_config.and_then(|config| config.fallback_interactive_args.as_deref()),
                    Self::resume_session_accent(cli_config),
                    Some(&session.name),
                    &existing_ids,
                    None,
                    cli_config.and_then(|config| config.model_flag.as_deref()),
                    None,
                ) {
                    Ok(agent) => {
                        used_args = fresh_args;
                        agent
                    }
                    Err(e2) => {
                        tracing::warn!(
                            "Fresh-session fallback also failed for '{}': {e2}; closing session",
                            session.name
                        );
                        // Neither the resume nor the fresh-launch attempt could
                        // start this CLI (binary missing, no resume flag and the
                        // original args no longer work, etc.) — leaving the row
                        // 'active' would strand it invisibly forever. There is
                        // no session-admin surface to revive it, so an
                        // unrecoverable session is simply dead: mark it closed
                        // (B32) so it disappears from the sidebar instead of
                        // lingering as a red, un-enterable orphan. The row is
                        // kept for history; `restore_scheduled_sends` drops any
                        // schedules that targeted it.
                        let _ = self.db.mark_session_closed(&session.id);
                        return;
                    }
                }
            }
        };

        // Mark the old session as 'resumed' before inserting its replacement.
        let _ = self.db.mark_session_resumed(&session.id);
        let _ = self.db.insert_interactive_session(
            &agent.id,
            &agent.name,
            cli.as_str(),
            &session.working_dir,
            used_args.as_deref(),
            agent.pid(),
            &session.session_type,
            current_boot_id,
        );
        // The resumed session gets a fresh runtime id; move any pending
        // scheduled sends from the old id onto it so they survive the restart.
        if let Err(e) = self.db.reassign_scheduled_sends(&session.id, &agent.id) {
            tracing::warn!(
                "Failed to reassign scheduled sends for resumed session '{}': {e}",
                session.name
            );
        }
        self.interactive_agents.push(agent);
    }

    /// Finalize scheduled-send restore after auto-resume: any pending schedule
    /// whose target session was not resumed (its session no longer exists) is
    /// dropped silently, then the delivery gate opens so due schedules — past
    /// due ones included — fire on the next tick. Idempotent; safe to call once
    /// on startup even when there are no sessions.
    pub fn restore_scheduled_sends(&mut self) {
        let live_ids: Vec<String> = self
            .interactive_agents
            .iter()
            .map(|agent| agent.id.clone())
            .collect();
        if let Err(e) = self.db.drop_scheduled_sends_missing_targets(&live_ids) {
            tracing::warn!("Failed to drop scheduled sends for missing sessions: {e}");
        }
        self.scheduled_sends_restored = true;
    }

    fn resume_terminal_session(
        &mut self,
        session: &crate::db::session::TerminalSession,
        cols: u16,
        rows: u16,
    ) {
        let existing_refs = self.terminal_agent_names();
        let agent = match InteractiveAgent::spawn_terminal(
            &session.shell,
            &session.working_dir,
            cols,
            rows,
            Some(&session.name),
            &existing_refs,
            self.theme.header_color,
        ) {
            Ok(agent) => agent,
            Err(e) => {
                tracing::warn!(
                    "Failed to auto-resume terminal session '{}': {e}",
                    session.name
                );
                return;
            }
        };

        let _ = self.db.insert_terminal_session(
            &agent.id,
            &agent.name,
            &session.shell,
            &session.working_dir,
        );
        let hist = super::terminal_history::load_history(&self.data_dir, &agent.name);
        agent.replay_scrollback_lines(&hist.scrollback);
        self.terminal_histories.insert(agent.name.clone(), hist);
        self.terminal_agents.push(agent);
    }

    /// Reap bridge sidecar rows left `active` by a process that died without
    /// calling `finish_standalone_session` (daemon restart, MCP client
    /// killed). Bridge sessions are never auto-resumed (see
    /// `get_active_sessions`), so without this their rows accumulate as
    /// `active` forever instead of just going stale.
    pub fn reconcile_bridge_sessions(&self) {
        let Ok(sessions) = self.db.get_active_sessions_by_type("bridge") else {
            return;
        };
        for session in &sessions {
            // Bridges are internal daemon processes — a live PID always means
            // the bridge is running, regardless of boot_id (unlike user CLI
            // sessions where PID recycling after reboot makes PIDs unreliable).
            let dead = match session.pid {
                Some(pid) => !process_is_alive(pid),
                None => true,
            };
            if dead {
                let _ = self.db.finish_interactive_session(&session.id, 1);
            }
        }
    }

    pub fn auto_resume_sessions(&mut self) {
        // Startup sweep (B32): retire any row still in the removed `orphaned`
        // status to `completed` so historic red orphan cards disappear. Runs
        // here, before this function's own resume attempts and before
        // `restore_scheduled_sends` (see `tui/mod.rs` startup order): a swept
        // session is not resumed, so it never joins `interactive_agents`, and
        // `restore_scheduled_sends`'s missing-target drop then discards any
        // `scheduled_sends` that still pointed at it.
        match self.db.close_orphaned_interactive_sessions() {
            Ok(n) if n > 0 => {
                tracing::info!("Closed {n} orphaned interactive session(s) on startup");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to sweep orphaned interactive sessions: {e}"),
        }

        let Ok(sessions) = self.db.get_active_sessions() else {
            return;
        };
        if sessions.is_empty() {
            tracing::info!("No active sessions to resume");
            return;
        }
        tracing::info!("Resuming {} active session(s)", sessions.len());

        let current_boot_id = crate::system::boot_id();
        let home = dirs::home_dir().unwrap_or_default();
        let canopy_config = crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy"));
        let (cols, rows) = Self::session_panel_size();

        for session in &sessions {
            if !should_resume_session(
                session.pid,
                session.boot_id.as_deref(),
                current_boot_id.as_deref(),
            ) {
                // The stored PID is alive on the same boot — but that alone is
                // NOT proof of a session-lock conflict. Two benign cases used
                // to permanently orphan healthy sessions here:
                //  * quick TUI close→reopen: the old CLI got its HUP and is
                //    still in the middle of dying;
                //  * PID recycling: the number now belongs to an unrelated
                //    process.
                // Only a live process that actually IS this CLI, and that
                // survives a short grace period, is a genuine conflict.
                let pid = session.pid.unwrap_or(0);
                if !process_outlives_grace(pid, &session.cli) {
                    tracing::info!(
                        "Auto-resuming session '{}': stored pid {pid} was recycled or exited during grace",
                        session.name
                    );
                    self.resume_interactive_session(
                        session,
                        &canopy_config,
                        cols,
                        rows,
                        current_boot_id.as_deref(),
                    );
                    continue;
                }
                tracing::warn!(
                    "Skipping auto-resume of session '{}': old process (pid {:?}) is still alive (same boot); closing session",
                    session.name,
                    session.pid
                );
                // A genuine session-lock conflict: the old CLI is still alive
                // and holding the lock, so this instance can never take the
                // session over. With no session-admin surface to hand it back,
                // it's dead to us — mark it closed (B32) so it drops from the
                // sidebar rather than lingering as a red orphan. Per-session,
                // so a crash right here strands at most this one row. The row
                // is kept for history; `restore_scheduled_sends` drops any
                // schedules that targeted it.
                let _ = self.db.mark_session_closed(&session.id);
                continue;
            }
            self.resume_interactive_session(
                session,
                &canopy_config,
                cols,
                rows,
                current_boot_id.as_deref(),
            );
        }

        if !self.interactive_agents.is_empty() {
            let _ = self.refresh_agents();
        }
    }

    pub fn auto_resume_terminal_sessions(&mut self) {
        let Ok(sessions) = self.db.get_active_terminal_sessions() else {
            return;
        };
        if sessions.is_empty() {
            tracing::info!("No active terminal sessions to resume");
            return;
        }
        tracing::info!("Resuming {} terminal session(s)", sessions.len());
        let _ = self.db.mark_orphaned_terminal_sessions();

        let (cols, rows) = Self::session_panel_size();

        for session in &sessions {
            self.resume_terminal_session(session, cols, rows);
        }

        if !self.terminal_agents.is_empty() {
            let _ = self.refresh_agents();
        }
    }
}

/// Check whether a process with the given pid is still alive.
///
/// Uses `kill(pid, 0)`: a `0` return means the process exists and is ours;
/// `EPERM` means it exists but is owned by someone else (still alive from our
/// point of view); `ESRCH` means it's gone.
#[cfg(unix)]
fn process_is_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: i64) -> bool {
    false
}

/// Whether the live process behind `pid` is actually an instance of `cli`.
/// `/proc/<pid>/comm` holds the executable's basename truncated to 15 bytes —
/// if it doesn't match, the PID was recycled by an unrelated process and the
/// session it came from is long gone.
#[cfg(target_os = "linux")]
fn process_matches_cli(pid: i64, cli: &str) -> bool {
    let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
        return false;
    };
    let want: String = cli.chars().take(15).collect();
    comm.trim() == want
}

/// Without procfs there is no cheap identity check — assume the PID is the
/// CLI so the conservative (grace-then-orphan) path handles it.
#[cfg(not(target_os = "linux"))]
fn process_matches_cli(_pid: i64, _cli: &str) -> bool {
    true
}

/// A stored-PID conflict is genuine only if the process is really this CLI
/// and it outlives a short grace window. A quick TUI close→reopen leaves the
/// old CLI mid-death for well under a second — waiting briefly turns what
/// used to be a permanent orphaning into a normal resume.
fn process_outlives_grace(pid: i64, cli: &str) -> bool {
    if pid <= 0 || !process_matches_cli(pid, cli) {
        return false;
    }
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        if !process_is_alive(pid) {
            return false;
        }
    }
    true
}

/// Whether an interactive session should be auto-resumed on startup.
///
/// Boot-id rule: if the stored boot_id is `None` (legacy row) or differs
/// from the current machine boot_id the PID is meaningless — after a reboot
/// the OS recycles PIDs from the bottom, so a stored PID matching a live
/// process is a coincidence, not evidence the original process survived.
/// In that case we always resume.
///
/// Only when the stored boot_id matches the current one do we fall back to
/// the PID-aliveness check: a live PID means the CLI is still running and
/// resuming would fight it for the session lock.
fn should_resume_session(
    pid: Option<i64>,
    session_boot_id: Option<&str>,
    current_boot_id: Option<&str>,
) -> bool {
    // Different boot (or legacy NULL) → PID is meaningless, always resume.
    match (session_boot_id, current_boot_id) {
        (Some(s), Some(c)) if s == c => {}
        _ => return true,
    }
    // Same boot → trust the PID-aliveness check.
    match pid {
        Some(pid) => !process_is_alive(pid),
        None => true,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SystemSample {
    cpu_usage: f32,
    mem_pct: f32,
    load: f32,
    cpu_temp: f32,
    gpu_usage: f32,
    gpu_temp: f32,
}

fn sample_from(info: &crate::system::SystemInfo) -> SystemSample {
    let mem_pct = if info.memory_total > 0 {
        (info.memory_used as f32 / info.memory_total as f32) * 100.0
    } else {
        0.0
    };

    SystemSample {
        cpu_usage: info.cpu_usage,
        mem_pct,
        load: info.load_average.unwrap_or(0.0) as f32,
        cpu_temp: info.cpu_temperature.unwrap_or(0.0),
        gpu_usage: info.gpu_info.as_ref().and_then(|g| g.usage).unwrap_or(0.0),
        gpu_temp: info
            .gpu_info
            .as_ref()
            .and_then(|g| g.temperature)
            .unwrap_or(0.0),
    }
}

fn adaptive_change_score(prev: SystemSample, next: SystemSample) -> f32 {
    let cpu_delta = (next.cpu_usage - prev.cpu_usage).abs() / 100.0;
    let mem_delta = (next.mem_pct - prev.mem_pct).abs() / 100.0;
    let load_delta = ((next.load - prev.load).abs() / 2.0).clamp(0.0, 1.0);
    let cpu_temp_delta = ((next.cpu_temp - prev.cpu_temp).abs() / 20.0).clamp(0.0, 1.0);
    let gpu_usage_delta = (next.gpu_usage - prev.gpu_usage).abs() / 100.0;
    let gpu_temp_delta = ((next.gpu_temp - prev.gpu_temp).abs() / 20.0).clamp(0.0, 1.0);

    cpu_delta
        .max(mem_delta)
        .max(load_delta)
        .max(cpu_temp_delta)
        .max(gpu_usage_delta)
        .max(gpu_temp_delta)
        .clamp(0.0, 1.0)
}

fn adaptive_poll_interval_ms(change_score: f32) -> u64 {
    const MIN_MS: f32 = 500.0;
    const MAX_MS: f32 = 3_000.0;
    let score = change_score.clamp(0.0, 1.0);
    (MAX_MS - ((MAX_MS - MIN_MS) * score)) as u64
}

fn lerp_f32(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

fn lerp_u64(from: u64, to: u64, t: f32) -> u64 {
    (from as f32 + (to as f32 - from as f32) * t).round() as u64
}

fn blend_optional_f32(current: Option<f32>, target: Option<f32>, t: f32) -> Option<f32> {
    match (current, target) {
        (Some(a), Some(b)) => Some(lerp_f32(a, b, t)),
        (_, value) => value,
    }
}

fn blend_optional_f64(current: Option<f64>, target: Option<f64>, t: f32) -> Option<f64> {
    match (current, target) {
        (Some(a), Some(b)) => Some(lerp_f32(a as f32, b as f32, t) as f64),
        (_, value) => value,
    }
}

fn blend_gpu_info(
    current: &Option<crate::system::GpuInfo>,
    target: &Option<crate::system::GpuInfo>,
    t: f32,
) -> Option<crate::system::GpuInfo> {
    match (current, target) {
        (Some(cur), Some(next)) => Some(crate::system::GpuInfo {
            name: if next.name.is_empty() {
                cur.name.clone()
            } else {
                next.name.clone()
            },
            vendor: if next.vendor.is_empty() {
                cur.vendor.clone()
            } else {
                next.vendor.clone()
            },
            usage: blend_optional_f32(cur.usage, next.usage, t),
            temperature: blend_optional_f32(cur.temperature, next.temperature, t),
            vram_used: match (cur.vram_used, next.vram_used) {
                (Some(a), Some(b)) => Some(lerp_u64(a, b, t)),
                (_, value) => value,
            },
            vram_total: next.vram_total.or(cur.vram_total),
            power_watts: blend_optional_f32(cur.power_watts, next.power_watts, t),
            power_limit_watts: next.power_limit_watts.or(cur.power_limit_watts),
        }),
        (_, value) => value.clone(),
    }
}

fn blend_system_info(
    current: &mut crate::system::SystemInfo,
    target: &crate::system::SystemInfo,
    t: f32,
) {
    current.cpu_usage = lerp_f32(current.cpu_usage, target.cpu_usage, t);
    current.cpu_cores = target.cpu_cores;
    current.cpu_temperature =
        blend_optional_f32(current.cpu_temperature, target.cpu_temperature, t);
    current.cpu_frequency_mhz = target.cpu_frequency_mhz;
    current.memory_used = lerp_u64(current.memory_used, target.memory_used, t);
    current.memory_total = target.memory_total;
    current.system_uptime = target.system_uptime;
    current.process_count = target.process_count;
    current.swap_used = lerp_u64(current.swap_used, target.swap_used, t);
    current.swap_total = target.swap_total;
    current.load_average = blend_optional_f64(current.load_average, target.load_average, t);
    current.gpu_info = blend_gpu_info(&current.gpu_info, &target.gpu_info, t);
    current.power_watts = blend_optional_f32(current.power_watts, target.power_watts, t);
    current.power_limit_watts = target.power_limit_watts;
    current.power_source = target.power_source;
}

/// Worker-thread body of the playground search (B23): loads/uses an
/// embedding client and queries the vector store on its own current-thread
/// runtime — a cold lazy model can take seconds to load, and none of that
/// may run on the UI thread.
fn playground_vector_search(
    query: &str,
    top_k: usize,
) -> anyhow::Result<Vec<crate::rag::vector_store::SearchResult>> {
    let canopy_dir = dirs::home_dir()
        .map(|h| h.join(".canopy"))
        .ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
    let model = config.embeddings_model.trim();
    if model.is_empty() {
        return Ok(Vec::new());
    }
    let dimensions = crate::rag::embedding_client::model_dimensions(model)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let store = crate::rag::vector_store::VectorStore::new(dimensions).await?;
        let embedder = crate::rag::embedding_client::client_from_config(&config)?;
        let query_vec = embedder.embed(query)?;
        store.search_similar(&query_vec, top_k).await
    })
}

fn spawn_system_monitor(
    system_monitor_active: &Arc<std::sync::atomic::AtomicBool>,
) -> std::sync::mpsc::Receiver<crate::system::SystemInfo> {
    let system_monitor_active_bg = Arc::clone(system_monitor_active);
    let (system_info_tx, system_info_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let initial = crate::system::SystemInfo::new();
        let mut previous_sample = sample_from(&initial);
        let mut poll_interval_ms = adaptive_poll_interval_ms(0.3);
        let mut was_active = true;
        let _ = system_info_tx.send(initial);

        loop {
            if !system_monitor_active_bg.load(Ordering::Relaxed) {
                was_active = false;
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }

            if !was_active {
                // Immediate catch-up sample after becoming visible again.
                let mut info = crate::system::SystemInfo::default();
                info.update();
                previous_sample = sample_from(&info);
                poll_interval_ms = adaptive_poll_interval_ms(0.6);
                let _ = system_info_tx.send(info);
                was_active = true;
            }

            std::thread::sleep(std::time::Duration::from_millis(poll_interval_ms));
            let mut info = crate::system::SystemInfo::default();
            info.update();

            let current_sample = sample_from(&info);
            let change_score = adaptive_change_score(previous_sample, current_sample);
            let target_ms = adaptive_poll_interval_ms(change_score) as f32;
            poll_interval_ms = (poll_interval_ms as f32 * 0.6 + target_ms * 0.4) as u64;
            previous_sample = current_sample;

            let _ = system_info_tx.send(info);
        }
    });
    system_info_rx
}

fn load_cli_usage() -> crate::domain::usage_stats::CliUsage {
    let mut usage = dirs::home_dir()
        .map(|h| crate::domain::usage_stats::CliUsage::load(&h.join(".canopy")))
        .unwrap_or_default();
    if usage.ensure_first_run() {
        let _ = dirs::home_dir().and_then(|h| usage.save(&h.join(".canopy")).ok().map(|_| ()));
    }
    usage
}

fn calculate_log_hash(raw_log: &str) -> u64 {
    raw_log.bytes().enumerate().fold(0u64, |acc, (idx, byte)| {
        acc.wrapping_add((byte as u64).wrapping_mul(idx as u64 + 1))
    })
}

fn log_contains_error(log_up: &str) -> bool {
    [
        "ERROR",
        "FAILED",
        "EXCEPTION",
        "PANIC",
        "SEGFAULT",
        "TIMED OUT",
        "CONNECTION REFUSED",
        "PERMISSION DENIED",
        "HALTED",
        "PROBLEMA",
        "FALLO",
        "FALLANDO",
    ]
    .iter()
    .any(|kw| log_up.contains(kw))
}

fn log_contains_success(log_up: &str) -> bool {
    [
        "SUCCESS",
        "ALL TESTS PASSED",
        "BUILD SUCCEEDED",
        "FINISHED",
        "COMPLETED",
        "DONE.",
        "STABILIZED",
        "READY",
        "CONVERGED",
        "DEPLOYED",
        "EXCELENTE",
        "COMPLETADO",
        "HECHO",
        "LISTO",
        "TERMINADO",
    ]
    .iter()
    .any(|kw| log_up.contains(kw))
}

fn log_contains_spawn(log_up: &str) -> bool {
    ["SPAWNING", "STARTING UP", "BOOTSTRAPPING", "INITIALIZING"]
        .iter()
        .any(|kw| log_up.contains(kw))
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::process_matches_cli;
    use super::{
        adaptive_change_score, adaptive_poll_interval_ms, blend_optional_f32, blend_optional_f64,
        build_resumed_session_args, calculate_log_hash, lerp_f32, lerp_u64, log_contains_error,
        log_contains_spawn, log_contains_success, process_is_alive, process_outlives_grace,
        sample_from, should_resume_session, SystemSample,
    };
    use crate::db::session::InteractiveSession;
    use crate::db::Database;
    use crate::tui::app::types::{AgentEntry, App, AutomationKind, ProjectTab, SidebarLayer};
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    #[test]
    fn reconcile_bridge_sessions_reaps_dead_pid_but_leaves_live_bridge_active() {
        let db = test_db();
        db.insert_interactive_session(
            "dead-bridge",
            "standalone",
            "bridge",
            "/tmp",
            Some("canopy bridge"),
            Some(999_999_999),
            "bridge",
            None,
        )
        .unwrap();
        db.insert_interactive_session(
            "live-bridge",
            "standalone",
            "bridge",
            "/tmp",
            Some("canopy bridge"),
            Some(std::process::id() as i64),
            "bridge",
            None,
        )
        .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.reconcile_bridge_sessions();

        let still_active = db.get_active_sessions_by_type("bridge").unwrap();
        assert_eq!(still_active.len(), 1);
        assert_eq!(still_active[0].id, "live-bridge");
    }

    #[test]
    fn test_yolo_mode_preservation_in_session_relaunch() {
        let session = InteractiveSession {
            id: "test-session".to_string(),
            name: "test-session".to_string(),
            cli: "opencode".to_string(),
            working_dir: "/tmp".to_string(),
            args: Some("--tui --yolo".to_string()),
            started_at: "2023-01-01T00:00:00Z".to_string(),
            status: "active".to_string(),
            session_type: "interactive".to_string(),
            pid: None,
            boot_id: None,
        };

        assert!(
            build_resumed_session_args(&session, None, None, None, Some("--yolo"))
                .as_deref()
                .is_some_and(|args| args.contains("--yolo"))
        );
    }

    #[test]
    fn test_yolo_flag_not_duplicated_when_falling_back_to_original_args() {
        let session = InteractiveSession {
            id: "test-session".to_string(),
            name: "test-session".to_string(),
            cli: "opencode".to_string(),
            working_dir: "/tmp".to_string(),
            args: Some("--tui --yolo".to_string()),
            started_at: "2023-01-01T00:00:00Z".to_string(),
            status: "active".to_string(),
            session_type: "interactive".to_string(),
            pid: None,
            boot_id: None,
        };

        let args = build_resumed_session_args(&session, None, None, None, Some("--yolo")).unwrap();
        assert_eq!(args.matches("--yolo").count(), 1);
    }

    #[test]
    fn test_original_resume_args_preserved_over_reconstructed_args() {
        let session = InteractiveSession {
            id: "test-session".to_string(),
            name: "test-session".to_string(),
            cli: "opencode".to_string(),
            working_dir: "/tmp".to_string(),
            args: Some("--session abc123 --yolo".to_string()),
            started_at: "2023-01-01T00:00:00Z".to_string(),
            status: "active".to_string(),
            session_type: "interactive".to_string(),
            pid: None,
            boot_id: None,
        };

        let args = build_resumed_session_args(
            &session,
            Some("--chat"),
            Some("-c"),
            Some("--session"),
            Some("--yolo"),
        )
        .unwrap();
        assert!(args.contains("--session abc123"));
        assert!(args.contains("--yolo"));
        assert!(!args.contains("-c"));
    }

    #[test]
    fn test_rebuilds_resume_args_for_fresh_session() {
        let session = InteractiveSession {
            id: "test-session".to_string(),
            name: "test-session".to_string(),
            cli: "copilot".to_string(),
            working_dir: "/tmp".to_string(),
            args: None,
            started_at: "2023-01-01T00:00:00Z".to_string(),
            status: "active".to_string(),
            session_type: "interactive".to_string(),
            pid: None,
            boot_id: None,
        };

        let args =
            build_resumed_session_args(&session, None, Some("--continue"), None, Some("--yolo"))
                .unwrap();
        assert!(args.contains("--continue"));
        assert!(!args.contains("--yolo"));
    }

    #[test]
    fn test_appends_resume_args_to_original_interactive_command() {
        let session = InteractiveSession {
            id: "test-session".to_string(),
            name: "test-session".to_string(),
            cli: "kiro".to_string(),
            working_dir: "/tmp".to_string(),
            args: Some("chat --trust-all-tools".to_string()),
            started_at: "2023-01-01T00:00:00Z".to_string(),
            status: "active".to_string(),
            session_type: "interactive".to_string(),
            pid: None,
            boot_id: None,
        };

        let args = build_resumed_session_args(
            &session,
            Some("chat"),
            Some("--resume-picker"),
            None,
            Some("--trust-all-tools"),
        )
        .unwrap();
        assert!(args.contains("chat"));
        assert!(args.contains("--resume-picker"));
        assert_eq!(args.matches("--trust-all-tools").count(), 1);
    }

    #[test]
    fn test_process_is_alive_for_own_pid() {
        assert!(process_is_alive(std::process::id() as i64));
    }

    #[test]
    fn test_process_is_alive_false_for_implausible_pid() {
        assert!(!process_is_alive(999_999_999));
    }

    #[test]
    fn test_should_resume_session_with_no_pid_always_resumes() {
        let current = crate::system::boot_id();
        assert!(should_resume_session(
            None,
            current.as_deref(),
            current.as_deref()
        ));
    }

    #[test]
    fn test_should_resume_session_skips_when_owner_process_is_alive() {
        let current = crate::system::boot_id();
        assert!(!should_resume_session(
            Some(std::process::id() as i64),
            current.as_deref(),
            current.as_deref()
        ));
    }

    #[test]
    fn test_should_resume_session_resumes_when_pid_is_gone() {
        let current = crate::system::boot_id();
        assert!(should_resume_session(
            Some(999_999_999),
            current.as_deref(),
            current.as_deref()
        ));
    }

    #[test]
    fn test_should_resume_session_resumes_on_boot_id_mismatch_even_with_live_pid() {
        let current = crate::system::boot_id();
        // Stored boot_id differs from current → PID is meaningless, always resume.
        assert!(should_resume_session(
            Some(std::process::id() as i64),
            Some("old-boot-id-from-previous-reboot"),
            current.as_deref(),
        ));
    }

    #[test]
    fn test_should_resume_session_resumes_when_stored_boot_id_is_null() {
        let current = crate::system::boot_id();
        // Legacy row with NULL boot_id → always resume.
        assert!(should_resume_session(
            Some(std::process::id() as i64),
            None,
            current.as_deref(),
        ));
    }

    #[test]
    fn test_should_resume_session_resumes_when_current_boot_id_is_none() {
        // Non-Linux host where boot_id can't be read → always resume.
        assert!(should_resume_session(
            Some(std::process::id() as i64),
            Some("some-stored-boot-id"),
            None,
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_process_matches_cli_detects_recycled_pids() {
        // Our own PID is alive but its comm is the test binary, not "claude" —
        // exactly the recycled-PID case that must NOT orphan a session.
        let own_pid = std::process::id() as i64;
        assert!(!process_matches_cli(own_pid, "claude"));

        // And it does match its own real comm.
        let own_comm = std::fs::read_to_string(format!("/proc/{own_pid}/comm"))
            .expect("read own comm")
            .trim()
            .to_string();
        assert!(process_matches_cli(own_pid, &own_comm));
    }

    #[test]
    fn test_process_outlives_grace_false_for_dead_or_recycled_pids() {
        // A PID nothing owns: resume immediately, no grace wait.
        assert!(!process_outlives_grace(-1, "claude"));
        // A live PID whose comm is another binary (recycled): also no wait.
        assert!(!process_outlives_grace(std::process::id() as i64, "claude"));
    }

    #[test]
    fn test_process_outlives_grace_waits_out_a_dying_process() {
        // A child that exits shortly after we check: the grace loop must
        // observe the death and report "no conflict" instead of orphaning.
        // The child is reaped on a side thread — an unreaped zombie would
        // still answer kill(pid, 0). (In production the contended PID never
        // belongs to a child of the new TUI, so there is no zombie window.)
        let mut child = std::process::Command::new("sleep")
            .arg("0.3")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i64;
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });
        assert!(!process_outlives_grace(pid, "sleep"));
        reaper.join().expect("join reaper");
    }

    // ── Sidebar: loops/backlog/history sections ─────────────────────

    fn make_project(hash: &str, path: &str) -> crate::domain::project::Project {
        crate::domain::project::Project {
            hash: hash.to_string(),
            path: path.to_string(),
            name: hash.to_string(),
            description: None,
            tags: None,
            indexed_at: None,
            created_at: 0,
        }
    }

    fn make_loop(
        id: &str,
        name: &str,
        status: crate::domain::loops::LoopStatus,
    ) -> crate::domain::loops::Loop {
        crate::domain::loops::Loop {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        }
    }

    fn make_backlog_spec(
        id: &str,
        name: &str,
        workdir: Option<&str>,
    ) -> crate::domain::loops::LoopSpec {
        crate::domain::loops::LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: name.to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: crate::domain::loops::LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: workdir.map(str::to_string),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    #[test]
    fn active_loops_orders_running_before_paused_before_draft() {
        use crate::domain::loops::LoopStatus;

        let db = test_db();
        db.insert_loop(&make_loop("l-draft", "Draft Loop", LoopStatus::Draft))
            .unwrap();
        db.insert_loop(&make_loop("l-done", "Done Loop", LoopStatus::Completed))
            .unwrap();
        db.insert_loop(&make_loop("l-paused", "Paused Loop", LoopStatus::Paused))
            .unwrap();
        db.insert_loop(&make_loop("l-running", "Running Loop", LoopStatus::Running))
            .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");

        let ids: Vec<&str> = app.active_loops().iter().map(|lp| lp.id.as_str()).collect();
        assert_eq!(ids, vec!["l-running", "l-paused", "l-draft"]);
        assert!(
            !ids.contains(&"l-done"),
            "completed loops must not appear in active_loops"
        );
    }

    #[test]
    fn refresh_backlog_specs_filters_by_selected_project_workdir() {
        let db = test_db();
        db.upsert_project(&make_project("hash0", "/tmp/proj0"))
            .unwrap();
        db.upsert_project(&make_project("hash1", "/tmp/proj1"))
            .unwrap();
        db.insert_loop_spec(&make_backlog_spec("spec-a", "Spec A", Some("/tmp/proj0")))
            .unwrap();
        db.insert_loop_spec(&make_backlog_spec("spec-b", "Spec B", Some("/tmp/proj1")))
            .unwrap();
        db.insert_loop_spec(&make_backlog_spec("spec-c", "Spec C", None))
            .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        assert_eq!(app.selected_project, 0);
        assert_eq!(
            app.backlog_specs
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["Spec A".to_string()],
            "backlog should be tag-filtered to the selected project's workdir"
        );

        app.selected_project = 1;
        app.refresh_backlog_specs().expect("refresh backlog");
        assert_eq!(
            app.backlog_specs
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["Spec B".to_string()]
        );
    }

    #[test]
    fn entering_a_project_defaults_to_overview_and_history_lazily_loads() {
        let db = test_db();
        db.upsert_project(&make_project("hash0", "/tmp/proj0"))
            .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        assert!(app.project_focus.is_none(), "starts on Preview, not Focus");

        app.enter_project_focus(ProjectTab::Overview);
        assert_eq!(app.project_focus, Some(ProjectTab::Overview));

        app.open_project_tab(ProjectTab::History);
        assert_eq!(app.project_focus, Some(ProjectTab::History));
        assert!(
            app.project_history_cache.contains_key("hash0"),
            "History tab lazily loads persisted data on first show"
        );

        app.exit_project_focus();
        assert!(app.project_focus.is_none());
    }

    #[test]
    fn cycle_project_tab_wraps_through_all_four_tabs() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.enter_project_focus(ProjectTab::Overview);

        app.cycle_project_tab(true);
        assert_eq!(app.project_focus, Some(ProjectTab::Backlog));
        app.cycle_project_tab(true);
        assert_eq!(app.project_focus, Some(ProjectTab::Knowledge));
        app.cycle_project_tab(true);
        assert_eq!(app.project_focus, Some(ProjectTab::History));
        app.cycle_project_tab(true);
        assert_eq!(app.project_focus, Some(ProjectTab::Overview));

        app.cycle_project_tab(false);
        assert_eq!(app.project_focus, Some(ProjectTab::History));
    }

    #[test]
    fn select_next_crosses_live_automation_knowledge_then_wraps() {
        use crate::domain::loops::LoopStatus;

        let db = test_db();
        db.upsert_project(&make_project("hash0", "/tmp/proj0"))
            .unwrap();
        db.insert_loop(&make_loop("l-active", "Active Loop", LoopStatus::Running))
            .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(crate::domain::models::Agent {
            id: "bg-1".to_string(),
            prompt: String::new(),
            trigger: None,
            cli: crate::domain::models::Cli::new("claude"),
            model: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: chrono::Utc::now(),
            log_path: "/tmp/bg-1.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        })];
        app.sidebar_layer = SidebarLayer::Automation;
        app.automation_kind = AutomationKind::Agent;
        app.selected = 0;

        // Automation's agent sub-list has one entry, so the next press
        // crosses into the loop sub-list before leaving the layer.
        app.select_next();
        assert_eq!(app.automation_kind, AutomationKind::Loop);
        assert_eq!(app.selected_loop_id.as_deref(), Some("l-active"));

        // Automation is exhausted — cross into Knowledge (the only project).
        app.select_next();
        assert_eq!(app.sidebar_layer, SidebarLayer::Knowledge);
        assert_eq!(app.selected_project, 0);

        // Knowledge is exhausted too — wrap back to the top of the ring.
        app.select_next();
        assert_eq!(app.sidebar_layer, SidebarLayer::Automation);
        assert_eq!(app.automation_kind, AutomationKind::Agent);
    }

    #[test]
    fn switch_sidebar_tab_lands_on_an_empty_tab_instead_of_refusing() {
        // Unlike `cycle_sidebar_layer` (which skips empty layers so
        // keyboard cycling never lands somewhere with nothing to select), a
        // deliberate mouse click on a tab must always switch to it — even
        // Automation with nothing running, so its empty state is reachable.
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        assert_eq!(app.sidebar_layer, SidebarLayer::Live);

        app.switch_sidebar_tab(SidebarLayer::Automation);

        assert_eq!(app.sidebar_layer, SidebarLayer::Automation);
    }

    #[test]
    fn switch_sidebar_tab_selects_first_item_when_the_tab_has_content() {
        let db = test_db();
        db.upsert_project(&make_project("hash0", "/tmp/proj0"))
            .unwrap();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");

        app.switch_sidebar_tab(SidebarLayer::Knowledge);

        assert_eq!(app.sidebar_layer, SidebarLayer::Knowledge);
        assert_eq!(app.selected_project, 0);
    }

    // ── Pure helper tests ────────────────────────────────────────

    #[test]
    fn calculate_log_hash_empty_string() {
        assert_eq!(calculate_log_hash(""), 0);
    }

    #[test]
    fn calculate_log_hash_deterministic() {
        let h1 = calculate_log_hash("hello world");
        let h2 = calculate_log_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn calculate_log_hash_different_inputs() {
        let h1 = calculate_log_hash("hello");
        let h2 = calculate_log_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn log_contains_error_positive() {
        assert!(log_contains_error("ERROR: something broke"));
        assert!(log_contains_error("FAILED to connect"));
        assert!(log_contains_error("EXCEPTION thrown"));
        assert!(log_contains_error("PANIC in module"));
        assert!(log_contains_error("SEGFAULT detected"));
        assert!(log_contains_error("TIMED OUT after 30s"));
        assert!(log_contains_error("CONNECTION REFUSED"));
        assert!(log_contains_error("PERMISSION DENIED"));
        assert!(log_contains_error("HALTED unexpectedly"));
        assert!(log_contains_error("PROBLEMA detectado"));
        assert!(log_contains_error("FALLO en el sistema"));
        assert!(log_contains_error("FALLANDO test"));
    }

    #[test]
    fn log_contains_error_negative() {
        assert!(!log_contains_error("everything is fine"));
        assert!(!log_contains_error("SUCCESS all done"));
        assert!(!log_contains_error(""));
    }

    #[test]
    fn log_contains_success_positive() {
        assert!(log_contains_success("SUCCESS"));
        assert!(log_contains_success("ALL TESTS PASSED"));
        assert!(log_contains_success("BUILD SUCCEEDED"));
        assert!(log_contains_success("FINISHED task"));
        assert!(log_contains_success("COMPLETED"));
        assert!(log_contains_success("DONE."));
        assert!(log_contains_success("STABILIZED"));
        assert!(log_contains_success("READY to deploy"));
        assert!(log_contains_success("CONVERGED"));
        assert!(log_contains_success("DEPLOYED to prod"));
        assert!(log_contains_success("EXCELENTE resultado"));
        assert!(log_contains_success("COMPLETADO"));
        assert!(log_contains_success("HECHO"));
        assert!(log_contains_success("LISTO"));
        assert!(log_contains_success("TERMINADO"));
    }

    #[test]
    fn log_contains_success_negative() {
        assert!(!log_contains_success("ERROR: failed"));
        assert!(!log_contains_success("running tests..."));
        assert!(!log_contains_success(""));
    }

    #[test]
    fn log_contains_spawn_positive() {
        assert!(log_contains_spawn("SPAWNING agent"));
        assert!(log_contains_spawn("STARTING UP server"));
        assert!(log_contains_spawn("BOOTSTRAPPING cluster"));
        assert!(log_contains_spawn("INITIALIZING module"));
    }

    #[test]
    fn log_contains_spawn_negative() {
        assert!(!log_contains_spawn("agent stopped"));
        assert!(!log_contains_spawn("DONE"));
        assert!(!log_contains_spawn(""));
    }

    #[test]
    fn lerp_f32_midpoint() {
        assert!((lerp_f32(0.0, 10.0, 0.5) - 5.0).abs() < f32::EPSILON);
    }

    #[test]
    fn lerp_f32_endpoints() {
        assert!((lerp_f32(0.0, 10.0, 0.0) - 0.0).abs() < f32::EPSILON);
        assert!((lerp_f32(0.0, 10.0, 1.0) - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn lerp_f32_negative() {
        assert!((lerp_f32(10.0, 0.0, 0.5) - 5.0).abs() < f32::EPSILON);
    }

    #[test]
    fn lerp_u64_midpoint() {
        assert_eq!(lerp_u64(0, 100, 0.5), 50);
    }

    #[test]
    fn lerp_u64_endpoints() {
        assert_eq!(lerp_u64(0, 100, 0.0), 0);
        assert_eq!(lerp_u64(0, 100, 1.0), 100);
    }

    #[test]
    fn blend_optional_f32_both_present() {
        assert_eq!(blend_optional_f32(Some(0.0), Some(10.0), 0.5), Some(5.0));
    }

    #[test]
    fn blend_optional_f32_first_none() {
        assert_eq!(blend_optional_f32(None, Some(10.0), 0.5), Some(10.0));
    }

    #[test]
    fn blend_optional_f32_second_none() {
        assert_eq!(blend_optional_f32(Some(0.0), None, 0.5), None);
    }

    #[test]
    fn blend_optional_f32_both_none() {
        assert_eq!(blend_optional_f32(None, None, 0.5), None);
    }

    #[test]
    fn blend_optional_f64_both_present() {
        let result = blend_optional_f64(Some(0.0), Some(10.0), 0.5);
        assert!((result.unwrap() - 5.0).abs() < 0.01);
    }

    #[test]
    fn blend_optional_f64_first_none() {
        assert_eq!(blend_optional_f64(None, Some(10.0), 0.5), Some(10.0));
    }

    #[test]
    fn adaptive_change_score_identical() {
        let s = SystemSample {
            cpu_usage: 50.0,
            mem_pct: 60.0,
            load: 1.0,
            cpu_temp: 40.0,
            gpu_usage: 30.0,
            gpu_temp: 50.0,
        };
        assert!((adaptive_change_score(s, s) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn adaptive_change_score_max_change() {
        let prev = SystemSample {
            cpu_usage: 0.0,
            mem_pct: 0.0,
            load: 0.0,
            cpu_temp: 0.0,
            gpu_usage: 0.0,
            gpu_temp: 0.0,
        };
        let next = SystemSample {
            cpu_usage: 100.0,
            mem_pct: 100.0,
            load: 2.0,
            cpu_temp: 20.0,
            gpu_usage: 100.0,
            gpu_temp: 20.0,
        };
        let score = adaptive_change_score(prev, next);
        assert!(score > 0.8);
        assert!(score <= 1.0);
    }

    #[test]
    fn adaptive_poll_interval_ms_fast_when_high_change() {
        let ms = adaptive_poll_interval_ms(1.0);
        assert!(ms <= 1000);
    }

    #[test]
    fn adaptive_poll_interval_ms_slow_when_no_change() {
        let ms = adaptive_poll_interval_ms(0.0);
        assert!(ms >= 2500);
    }

    #[test]
    fn adaptive_poll_interval_ms_clamped() {
        let ms_low = adaptive_poll_interval_ms(-1.0);
        let ms_high = adaptive_poll_interval_ms(2.0);
        assert!(ms_low >= 500);
        assert!(ms_high <= 3000);
    }

    #[test]
    fn sample_from_basic_conversion() {
        let info = crate::system::SystemInfo {
            cpu_usage: 42.5,
            memory_used: 4_000_000_000,
            memory_total: 8_000_000_000,
            load_average: Some(1.5),
            cpu_temperature: Some(55.0),
            gpu_info: Some(crate::system::GpuInfo {
                name: "RTX 4090".to_string(),
                vendor: "NVIDIA".to_string(),
                usage: Some(70.0),
                temperature: Some(65.0),
                vram_used: Some(8_000_000_000),
                vram_total: Some(24_000_000_000),
                power_watts: Some(300.0),
                power_limit_watts: Some(450.0),
            }),
            ..crate::system::SystemInfo::default()
        };
        let sample = sample_from(&info);
        assert!((sample.cpu_usage - 42.5).abs() < f32::EPSILON);
        assert!((sample.mem_pct - 50.0).abs() < 0.1);
        assert!((sample.load - 1.5).abs() < f32::EPSILON);
        assert!((sample.cpu_temp - 55.0).abs() < f32::EPSILON);
        assert!((sample.gpu_usage - 70.0).abs() < f32::EPSILON);
        assert!((sample.gpu_temp - 65.0).abs() < f32::EPSILON);
    }

    #[test]
    fn sample_from_zero_memory_total() {
        let info = crate::system::SystemInfo {
            memory_used: 1000,
            memory_total: 0,
            ..crate::system::SystemInfo::default()
        };
        let sample = sample_from(&info);
        assert!((sample.mem_pct).abs() < f32::EPSILON);
    }

    #[test]
    fn sample_from_no_gpu() {
        let info = crate::system::SystemInfo {
            gpu_info: None,
            ..crate::system::SystemInfo::default()
        };
        let sample = sample_from(&info);
        assert!((sample.gpu_usage).abs() < f32::EPSILON);
        assert!((sample.gpu_temp).abs() < f32::EPSILON);
    }

    #[test]
    fn sidebar_tab_index_returns_correct_index() {
        assert_eq!(App::sidebar_tab_index(SidebarLayer::Live), 0);
        assert_eq!(App::sidebar_tab_index(SidebarLayer::Automation), 1);
        assert_eq!(App::sidebar_tab_index(SidebarLayer::Knowledge), 2);
    }

    #[test]
    fn update_prompt_config_on_object() {
        let config = serde_json::json!({"platform": "claude", "model": "sonnet"});
        let result = App::update_prompt_config(&config, "new prompt here");
        assert_eq!(result["prompt_template"], "new prompt here");
        assert_eq!(result["platform"], "claude");
        assert_eq!(result["model"], "sonnet");
    }

    #[test]
    fn update_prompt_config_on_non_object() {
        let config = serde_json::json!("just a string");
        let result = App::update_prompt_config(&config, "prompt text");
        assert_eq!(result["prompt_template"], "prompt text");
    }

    #[test]
    fn update_prompt_config_preserves_existing_prompt_template() {
        let config = serde_json::json!({"prompt_template": "old prompt"});
        let result = App::update_prompt_config(&config, "replaced");
        assert_eq!(result["prompt_template"], "replaced");
    }

    #[test]
    fn update_prompt_config_empty_object() {
        let config = serde_json::json!({});
        let result = App::update_prompt_config(&config, "test");
        assert_eq!(result["prompt_template"], "test");
    }

    #[test]
    fn terminal_selection_normalized_ordering() {
        let sel = crate::tui::app::types::TerminalSelection {
            agent: (true, 0),
            start: (5, 10),
            end: (2, 3),
            dragging: false,
        };
        let (s, e) = sel.normalized();
        assert_eq!(s, (2, 3));
        assert_eq!(e, (5, 10));
    }

    #[test]
    fn terminal_selection_normalized_already_ordered() {
        let sel = crate::tui::app::types::TerminalSelection {
            agent: (false, 1),
            start: (1, 2),
            end: (3, 4),
            dragging: false,
        };
        let (s, e) = sel.normalized();
        assert_eq!(s, (1, 2));
        assert_eq!(e, (3, 4));
    }

    #[test]
    fn terminal_selection_normalized_equal_endpoints() {
        let sel = crate::tui::app::types::TerminalSelection {
            agent: (true, 0),
            start: (2, 3),
            end: (2, 3),
            dragging: false,
        };
        let (s, e) = sel.normalized();
        assert_eq!(s, (2, 3));
        assert_eq!(e, (2, 3));
    }

    // ── LoopEditorDialog tests ──────────────────────────────────

    #[test]
    fn loop_editor_dialog_new_sets_cursor_at_end() {
        let dialog = crate::tui::app::types::LoopEditorDialog::new(
            "n1".into(),
            "node1".into(),
            "title".into(),
            "help".into(),
            "hello world".into(),
            crate::tui::app::types::LoopEditorMode::AgentPrompt,
        );
        assert_eq!(dialog.cursor, 11); // "hello world" has 11 chars
    }

    #[test]
    fn loop_editor_dialog_char_len() {
        let mut dialog = crate::tui::app::types::LoopEditorDialog::new(
            "n1".into(),
            "node1".into(),
            "".into(),
            "".into(),
            "abc".into(),
            crate::tui::app::types::LoopEditorMode::NodeConfig,
        );
        assert_eq!(dialog.char_len(), 3);
        dialog.insert_str("de");
        assert_eq!(dialog.char_len(), 5);
    }

    #[test]
    fn loop_editor_dialog_insert_char() {
        let mut dialog = crate::tui::app::types::LoopEditorDialog::new(
            "n1".into(),
            "node1".into(),
            "".into(),
            "".into(),
            "ac".into(),
            crate::tui::app::types::LoopEditorMode::AgentPrompt,
        );
        dialog.cursor = 1;
        dialog.insert_char('b');
        assert_eq!(dialog.buffer, "abc");
        assert_eq!(dialog.cursor, 2);
    }

    #[test]
    fn loop_editor_dialog_backspace() {
        let mut dialog = crate::tui::app::types::LoopEditorDialog::new(
            "n1".into(),
            "node1".into(),
            "".into(),
            "".into(),
            "abc".into(),
            crate::tui::app::types::LoopEditorMode::AgentPrompt,
        );
        dialog.backspace();
        assert_eq!(dialog.buffer, "ab");
        assert_eq!(dialog.cursor, 2);
    }

    #[test]
    fn loop_editor_dialog_backspace_at_zero() {
        let mut dialog = crate::tui::app::types::LoopEditorDialog::new(
            "n1".into(),
            "node1".into(),
            "".into(),
            "".into(),
            "abc".into(),
            crate::tui::app::types::LoopEditorMode::AgentPrompt,
        );
        dialog.cursor = 0;
        dialog.backspace();
        assert_eq!(dialog.buffer, "abc");
    }

    #[test]
    fn loop_editor_dialog_move_left_right() {
        let mut dialog = crate::tui::app::types::LoopEditorDialog::new(
            "n1".into(),
            "node1".into(),
            "".into(),
            "".into(),
            "abc".into(),
            crate::tui::app::types::LoopEditorMode::AgentPrompt,
        );
        dialog.move_left();
        assert_eq!(dialog.cursor, 2);
        dialog.move_right();
        assert_eq!(dialog.cursor, 3);
        dialog.move_right(); // At end
        assert_eq!(dialog.cursor, 3);
    }

    #[test]
    fn loop_editor_dialog_move_home_end() {
        let mut dialog = crate::tui::app::types::LoopEditorDialog::new(
            "n1".into(),
            "node1".into(),
            "".into(),
            "".into(),
            "hello".into(),
            crate::tui::app::types::LoopEditorMode::AgentPrompt,
        );
        dialog.move_home();
        assert_eq!(dialog.cursor, 0);
        dialog.move_end();
        assert_eq!(dialog.cursor, 5);
    }

    // ── Knowledge filter tests ──────────────────────────────────

    #[test]
    fn filtered_knowledge_indices_empty_filter_returns_all() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.project_knowledge = vec![
            crate::db::intelligence::IntelligenceNodeRecord {
                id: "n1".into(),
                kind: "fact".into(),
                title: "Fact One".into(),
                body: "body one".into(),
                metadata: None,
                project_hash: None,
                session_id: None,
                created_at: chrono::Utc::now().timestamp(),
                updated_at: chrono::Utc::now().timestamp(),
            },
            crate::db::intelligence::IntelligenceNodeRecord {
                id: "n2".into(),
                kind: "pattern".into(),
                title: "Pattern Two".into(),
                body: "body two".into(),
                metadata: None,
                project_hash: None,
                session_id: None,
                created_at: chrono::Utc::now().timestamp(),
                updated_at: chrono::Utc::now().timestamp(),
            },
        ];
        app.knowledge_filter.clear();
        let indices = app.filtered_knowledge_indices();
        assert_eq!(indices, vec![0, 1]);
    }

    #[test]
    fn filtered_knowledge_indices_filter_matches_title() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.project_knowledge = vec![
            crate::db::intelligence::IntelligenceNodeRecord {
                id: "n1".into(),
                kind: "fact".into(),
                title: "Rust Ownership".into(),
                body: "body".into(),
                metadata: None,
                project_hash: None,
                session_id: None,
                created_at: chrono::Utc::now().timestamp(),
                updated_at: chrono::Utc::now().timestamp(),
            },
            crate::db::intelligence::IntelligenceNodeRecord {
                id: "n2".into(),
                kind: "fact".into(),
                title: "Python GIL".into(),
                body: "body".into(),
                metadata: None,
                project_hash: None,
                session_id: None,
                created_at: chrono::Utc::now().timestamp(),
                updated_at: chrono::Utc::now().timestamp(),
            },
        ];
        app.knowledge_filter = "rust".to_string();
        let indices = app.filtered_knowledge_indices();
        assert_eq!(indices, vec![0]);
    }

    #[test]
    fn filtered_knowledge_indices_filter_matches_body() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.project_knowledge = vec![
            crate::db::intelligence::IntelligenceNodeRecord {
                id: "n1".into(),
                kind: "fact".into(),
                title: "Title".into(),
                body: "contains the word pattern".into(),
                metadata: None,
                project_hash: None,
                session_id: None,
                created_at: chrono::Utc::now().timestamp(),
                updated_at: chrono::Utc::now().timestamp(),
            },
        ];
        app.knowledge_filter = "pattern".to_string();
        let indices = app.filtered_knowledge_indices();
        assert_eq!(indices, vec![0]);
    }

    #[test]
    fn filtered_knowledge_indices_filter_matches_kind() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.project_knowledge = vec![
            crate::db::intelligence::IntelligenceNodeRecord {
                id: "n1".into(),
                kind: "fact".into(),
                title: "Title".into(),
                body: "body".into(),
                metadata: None,
                project_hash: None,
                session_id: None,
                created_at: chrono::Utc::now().timestamp(),
                updated_at: chrono::Utc::now().timestamp(),
            },
            crate::db::intelligence::IntelligenceNodeRecord {
                id: "n2".into(),
                kind: "pattern".into(),
                title: "Title".into(),
                body: "body".into(),
                metadata: None,
                project_hash: None,
                session_id: None,
                created_at: chrono::Utc::now().timestamp(),
                updated_at: chrono::Utc::now().timestamp(),
            },
        ];
        app.knowledge_filter = "pattern".to_string();
        let indices = app.filtered_knowledge_indices();
        assert_eq!(indices, vec![1]);
    }

    #[test]
    fn filtered_knowledge_indices_no_match() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.project_knowledge = vec![crate::db::intelligence::IntelligenceNodeRecord {
            id: "n1".into(),
            kind: "fact".into(),
            title: "Title".into(),
            body: "body".into(),
            metadata: None,
            project_hash: None,
            session_id: None,
            created_at: chrono::Utc::now().timestamp(),
            updated_at: chrono::Utc::now().timestamp(),
        }];
        app.knowledge_filter = "zzz_not_found".to_string();
        let indices = app.filtered_knowledge_indices();
        assert!(indices.is_empty());
    }

    // ── ProjectTab tests ────────────────────────────────────────

    #[test]
    fn project_tab_labels() {
        assert_eq!(ProjectTab::Overview.label(), "Overview");
        assert_eq!(ProjectTab::Backlog.label(), "Backlog");
        assert_eq!(ProjectTab::Knowledge.label(), "Knowledge");
        assert_eq!(ProjectTab::History.label(), "History");
    }

    #[test]
    fn project_tab_hotkeys() {
        assert_eq!(ProjectTab::Overview.hotkey(), 'o');
        assert_eq!(ProjectTab::Backlog.hotkey(), 'b');
        assert_eq!(ProjectTab::Knowledge.hotkey(), 'k');
        assert_eq!(ProjectTab::History.hotkey(), 'h');
    }

    #[test]
    fn project_tab_all_has_four_entries() {
        assert_eq!(ProjectTab::ALL.len(), 4);
    }

    // ── AgentEntry::id tests ────────────────────────────────────

    #[test]
    fn agent_entry_id_for_agent() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let entry = AgentEntry::Agent(crate::domain::models::Agent {
            id: "bg-1".to_string(),
            prompt: String::new(),
            trigger: None,
            cli: crate::domain::models::Cli::new("claude"),
            model: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: chrono::Utc::now(),
            log_path: "/tmp/bg-1.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        });
        assert_eq!(entry.id(&app), "bg-1");
    }

    #[test]
    fn agent_entry_id_for_corrupt() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let entry = AgentEntry::Corrupt(crate::domain::models::CorruptAgent {
            id: "corrupt-1".to_string(),
            enabled: false,
            error: "corrupt row".to_string(),
        });
        assert_eq!(entry.id(&app), "corrupt-1");
    }

    #[test]
    fn agent_entry_id_for_group_out_of_bounds() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let entry = AgentEntry::Group(99);
        assert_eq!(entry.id(&app), "?");
    }

    #[test]
    fn agent_entry_id_for_interactive_out_of_bounds() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let entry = AgentEntry::Interactive(99);
        assert_eq!(entry.id(&app), "?");
    }

    #[test]
    fn agent_entry_id_for_terminal_out_of_bounds() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let entry = AgentEntry::Terminal(99);
        assert_eq!(entry.id(&app), "?");
    }

    #[test]
    fn agent_entry_id_for_orphaned_out_of_bounds() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let entry = AgentEntry::Orphaned(99);
        assert_eq!(entry.id(&app), "?");
    }

    // ── Navigation edge cases ───────────────────────────────────

    #[test]
    fn select_next_empty_agents_stays_put() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents.clear();
        app.selected = 0;
        app.sidebar_layer = SidebarLayer::Live;
        app.select_next();
        // Should not panic, stays at 0
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn select_prev_empty_agents_stays_put() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents.clear();
        app.selected = 0;
        app.sidebar_layer = SidebarLayer::Live;
        app.select_prev();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn select_agent_at_out_of_bounds_does_nothing() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents.clear();
        let prev = app.selected;
        app.select_agent_at(999);
        assert_eq!(app.selected, prev);
    }

    #[test]
    fn scroll_log_down_and_up() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.log_scroll = 10;
        app.scroll_log_down();
        assert_eq!(app.log_scroll, 13);
        app.scroll_log_up();
        assert_eq!(app.log_scroll, 10);
    }

    #[test]
    fn scroll_log_up_at_zero_stays_zero() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.log_scroll = 0;
        app.scroll_log_up();
        assert_eq!(app.log_scroll, 0);
    }

    // ── active_loops filtering ───────────────────────────────────

    #[test]
    fn active_loops_excludes_completed_and_failed() {
        use crate::domain::loops::LoopStatus;
        let db = test_db();
        db.insert_loop(&make_loop("l1", "Running", LoopStatus::Running))
            .unwrap();
        db.insert_loop(&make_loop("l2", "Draft", LoopStatus::Draft))
            .unwrap();
        db.insert_loop(&make_loop("l3", "Paused", LoopStatus::Paused))
            .unwrap();
        db.insert_loop(&make_loop("l4", "Completed", LoopStatus::Completed))
            .unwrap();
        db.insert_loop(&make_loop("l5", "Failed", LoopStatus::Failed))
            .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let active: Vec<&str> = app.active_loops().iter().map(|lp| lp.id.as_str()).collect();
        assert!(!active.contains(&"l4"));
        assert!(!active.contains(&"l5"));
        assert!(active.contains(&"l1"));
        assert!(active.contains(&"l2"));
        assert!(active.contains(&"l3"));
    }

    // ── Playground state tests ───────────────────────────────────

    #[test]
    fn activate_deactivate_playground() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        assert!(!app.playground_active);

        app.playground_query = "test query".to_string();
        app.playground_selected = 5;
        app.playground_active = true;
        app.activate_playground();
        assert!(app.playground_active);
        assert!(app.playground_query.is_empty());
        assert_eq!(app.playground_selected, 0);

        app.deactivate_playground();
        assert!(!app.playground_active);
        assert!(app.playground_query.is_empty());
    }

    // ── Playground search edge cases ─────────────────────────────

    #[test]
    fn poll_playground_search_no_rx_returns_early() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.playground_search_rx = None;
        // Should not panic
        app.poll_playground_search();
    }

    #[test]
    fn poll_playground_search_disconnected_cleans_up() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let (tx, rx) = std::sync::mpsc::channel();
        app.playground_search_rx = Some(rx);
        app.playground_search_pending = true;
        drop(tx);
        app.poll_playground_search();
        assert!(app.playground_search_rx.is_none());
        assert!(!app.playground_search_pending);
    }

    #[test]
    fn duplicate_selected_loop_node_copies_config_and_opens_editor() {
        use crate::domain::loops::{LoopNode, LoopNodeKind, LoopSpec, LoopSpecStatus, LoopStatus};
        use crate::tui::app::types::Focus;

        let db = test_db();
        db.insert_loop(&make_loop("loop-1", "Loop", LoopStatus::Draft))
            .unwrap();
        db.insert_loop_spec(&LoopSpec {
            id: "spec-a".to_string(),
            loop_id: Some("loop-1".to_string()),
            name: "spec-a".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "impl".to_string(),
            spec_id: Some("spec-a".to_string()),
            loop_id: None,
            name: "implement".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude", "prompt_template": "do it"}),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.selected_loop_id = Some("loop-1".to_string());
        app.loop_details = db.get_loop_details("loop-1").unwrap();
        app.loop_selected_spec = 0;
        app.loop_selected_node = 0;
        assert_eq!(app.selected_loop_node().unwrap().id, "impl");

        app.duplicate_selected_loop_node().unwrap();

        // The editor opens pre-filled on the new copy.
        assert!(matches!(app.focus, Focus::LoopEditorDialog));
        let dialog = app.loop_editor_dialog.as_ref().unwrap();
        assert!(
            dialog.node_name.ends_with("(copy)"),
            "expected a copy name, got '{}'",
            dialog.node_name
        );
        assert_ne!(dialog.node_id, "impl", "the copy must have a fresh id");

        // A second node now exists on the spec with the source's config.
        let nodes = db.list_loop_nodes("spec-a").unwrap();
        assert_eq!(nodes.len(), 2);
        let copy = nodes.iter().find(|n| n.id != "impl").unwrap();
        assert_eq!(copy.name, "implement (copy)");
        assert_eq!(copy.config["platform"], "claude");
        assert_eq!(copy.config["prompt_template"], "do it");
    }
}
