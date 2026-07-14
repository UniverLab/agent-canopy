mod agents;
mod data;
pub mod dialog;
mod gamification;
mod loop_live_state;
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
pub use types::{AgentEntry, AgentSectionFocus, App, Focus, ProjectsPanelFocus, SidebarMode};
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
            sidebar_mode: SidebarMode::Agents,
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
            projects_panel_focus: ProjectsPanelFocus::Projects,
            agent_section_focus: AgentSectionFocus::Background,
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
            backlog_specs: Vec::new(),
            selected_backlog: 0,
            history_collapsed: true,
            selected_history: 0,
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
            notifications_enabled: true,
            notification_service: Arc::new(DefaultNotificationService),
            prev_active_run_ids: std::collections::HashSet::new(),
            animation_tick: 0,
            temperature_unit: canopy_config.temperature_unit,
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

    /// Perform debounced RAG search in playground mode
    fn refresh_playground_search(&mut self) -> Result<()> {
        const PLAYGROUND_SEARCH_DEBOUNCE_MS: u128 = 2_000;

        if !self.playground_active {
            return Ok(());
        }
        if !self.playground_search_pending {
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

        if let Ok(results) = self.rag_vector_search(&query, 50) {
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
        self.playground_last_executed_query = query;
        self.playground_search_pending = false;
        Ok(())
    }

    // ── Navigation ──────────────────────────────────────────────

    pub fn select_next(&mut self) {
        if self.sidebar_mode == SidebarMode::Projects {
            self.select_next_project_panel();
            return;
        }

        self.select_next_agent_panel();
    }

    fn select_next_project_panel(&mut self) {
        self.normalize_projects_panel_focus();
        match self.projects_panel_focus {
            ProjectsPanelFocus::Projects => self.navigate_projects_next(),
            ProjectsPanelFocus::Loops => self.navigate_active_loops_next(),
            ProjectsPanelFocus::Backlog => self.navigate_backlog_next(),
            ProjectsPanelFocus::History => self.navigate_history_next(),
            ProjectsPanelFocus::Knowledge => self.navigate_knowledge_next(),
            ProjectsPanelFocus::RagInfo => self.navigate_from_rag_info_next(),
        }
        self.reset_log_scroll();
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

    fn navigate_projects_next(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        let next = self.selected_project + 1;
        if next < self.projects.len() {
            self.selected_project = next;
            self.refresh_loops_selection();
            return;
        }
        self.cross_forward_from_projects();
    }

    /// Shared "ran off the end" fallback for Projects/Loops/Backlog: try
    /// entering the next non-empty section in sidebar order (Loops →
    /// Backlog → History → RagInfo), else wrap to the first project.
    fn cross_forward_from_projects(&mut self) {
        if self.try_cross_to_loops_first() {
            return;
        }
        self.cross_forward_from_loops();
    }

    fn cross_forward_from_loops(&mut self) {
        if self.try_cross_to_backlog_first() {
            return;
        }
        self.cross_forward_from_backlog();
    }

    fn cross_forward_from_backlog(&mut self) {
        if self.try_cross_to_history_first() {
            return;
        }
        if self.rag_info.has_rag_activity() {
            self.projects_panel_focus = ProjectsPanelFocus::RagInfo;
            return;
        }
        self.projects_panel_focus = ProjectsPanelFocus::Projects;
        self.selected_project = 0;
    }

    fn try_cross_to_loops_first(&mut self) -> bool {
        let Some(id) = self.active_loops().first().map(|lp| lp.id.clone()) else {
            return false;
        };
        self.projects_panel_focus = ProjectsPanelFocus::Loops;
        self.selected_loop_id = Some(id);
        self.refresh_loops_selection();
        true
    }

    fn try_cross_to_backlog_first(&mut self) -> bool {
        if self.backlog_specs.is_empty() {
            return false;
        }
        self.projects_panel_focus = ProjectsPanelFocus::Backlog;
        self.selected_backlog = 0;
        true
    }

    /// Focuses `History` on its first finished loop. When the section is
    /// collapsed there is nothing to select yet — focus lands on the header
    /// (Enter/→ expands it) without disturbing `selected_loop_id`.
    fn try_cross_to_history_first(&mut self) -> bool {
        let finished_ids: Vec<String> = self
            .finished_loops()
            .iter()
            .map(|lp| lp.id.clone())
            .collect();
        if finished_ids.is_empty() {
            return false;
        }
        self.projects_panel_focus = ProjectsPanelFocus::History;
        if !self.history_collapsed {
            self.selected_history = 0;
            self.selected_loop_id = Some(finished_ids[0].clone());
            self.refresh_loops_selection();
        }
        true
    }

    fn navigate_active_loops_next(&mut self) {
        let visible_ids: Vec<String> = self
            .active_loops()
            .into_iter()
            .map(|lp| lp.id.clone())
            .collect();
        if visible_ids.is_empty() {
            self.cross_forward_from_loops();
            return;
        }
        let current = self
            .selected_loop_id
            .as_ref()
            .and_then(|id| visible_ids.iter().position(|vid| vid == id))
            .unwrap_or(0);
        let next = current + 1;
        if next < visible_ids.len() {
            self.selected_loop_id = Some(visible_ids[next].clone());
            self.refresh_loops_selection();
            return;
        }
        self.cross_forward_from_loops();
    }

    fn navigate_backlog_next(&mut self) {
        if self.backlog_specs.is_empty() {
            self.cross_forward_from_backlog();
            return;
        }
        let next = self.selected_backlog + 1;
        if next < self.backlog_specs.len() {
            self.selected_backlog = next;
            return;
        }
        self.cross_forward_from_backlog();
    }

    fn navigate_history_next(&mut self) {
        let finished_ids: Vec<String> = self
            .finished_loops()
            .iter()
            .map(|lp| lp.id.clone())
            .collect();
        if finished_ids.is_empty() || self.history_collapsed {
            if self.rag_info.has_rag_activity() {
                self.projects_panel_focus = ProjectsPanelFocus::RagInfo;
                return;
            }
            self.projects_panel_focus = ProjectsPanelFocus::Projects;
            self.selected_project = 0;
            return;
        }
        let current = self
            .selected_loop_id
            .as_ref()
            .and_then(|id| finished_ids.iter().position(|vid| vid == id))
            .unwrap_or(0);
        let next = current + 1;
        if next < finished_ids.len() {
            self.selected_history = next;
            self.selected_loop_id = Some(finished_ids[next].clone());
            self.refresh_loops_selection();
            return;
        }
        if self.rag_info.has_rag_activity() {
            self.projects_panel_focus = ProjectsPanelFocus::RagInfo;
            return;
        }
        self.projects_panel_focus = ProjectsPanelFocus::Projects;
        self.selected_project = 0;
    }

    fn navigate_from_rag_info_next(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        self.projects_panel_focus = ProjectsPanelFocus::Projects;
        self.selected_project = 0;
        self.refresh_loops_selection();
    }

    fn select_next_agent_panel(&mut self) {
        if !self.rag_info.has_rag_activity() {
            self.advance_agent_selection();
            return;
        }

        if self.agents_rag_focused {
            self.agents_rag_focused = false;
            if !self.agents.is_empty() {
                let prev = self.selected;
                self.selected = 0;
                self.update_agent_section_focus_on_change(prev);
            }
            self.reset_log_scroll();
            return;
        }

        if self.agents.is_empty() || self.selected + 1 >= self.agents.len() {
            self.agents_rag_focused = true;
            self.reset_log_scroll();
            return;
        }

        self.advance_agent_selection();
    }

    fn advance_agent_selection(&mut self) {
        if self.agents.is_empty() {
            return;
        }

        let prev = self.selected;
        self.selected = (self.selected + 1) % self.agents.len();
        self.update_agent_section_focus_on_change(prev);
        self.reset_log_scroll();
    }

    fn update_agent_section_focus_on_change(&mut self, _prev_selected: usize) {
        if let Some(agent) = self.agents.get(self.selected) {
            self.agent_section_focus = match agent {
                AgentEntry::Agent(_) | AgentEntry::Corrupt(_) | AgentEntry::Group(_) => {
                    AgentSectionFocus::Background
                }
                AgentEntry::Interactive(_) | AgentEntry::Orphaned(_) => {
                    AgentSectionFocus::Interactive
                }
                AgentEntry::Terminal(_) => AgentSectionFocus::Terminal,
            };
        }
    }

    pub fn select_prev(&mut self) {
        if self.sidebar_mode == SidebarMode::Projects {
            self.select_prev_project_panel();
            return;
        }

        self.select_prev_agent_panel();
    }

    fn select_prev_project_panel(&mut self) {
        self.normalize_projects_panel_focus();
        match self.projects_panel_focus {
            ProjectsPanelFocus::Projects => self.navigate_projects_prev(),
            ProjectsPanelFocus::Loops => self.navigate_active_loops_prev(),
            ProjectsPanelFocus::Backlog => self.navigate_backlog_prev(),
            ProjectsPanelFocus::History => self.navigate_history_prev(),
            ProjectsPanelFocus::Knowledge => self.navigate_knowledge_prev(),
            ProjectsPanelFocus::RagInfo => self.navigate_from_rag_info_prev(),
        }
        self.reset_log_scroll();
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

    fn navigate_projects_prev(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        if self.selected_project > 0 {
            self.selected_project -= 1;
            self.refresh_loops_selection();
            return;
        }
        self.cross_backward_from_projects();
    }

    /// Shared "ran off the start" fallback for Projects: try entering the
    /// previous non-empty section in reverse sidebar order (RagInfo →
    /// History → Backlog → Loops), else wrap to the last project.
    fn cross_backward_from_projects(&mut self) {
        if self.rag_info.has_rag_activity() {
            self.projects_panel_focus = ProjectsPanelFocus::RagInfo;
            return;
        }
        if self.try_cross_to_history_last() {
            return;
        }
        if self.try_cross_to_backlog_last() {
            return;
        }
        if self.try_cross_to_loops_last() {
            return;
        }
        self.selected_project = self.projects.len() - 1;
    }

    fn cross_backward_from_loops(&mut self) {
        if !self.projects.is_empty() {
            self.projects_panel_focus = ProjectsPanelFocus::Projects;
            self.selected_project = self.projects.len() - 1;
            return;
        }
        // No projects to land on either — stay put on the last active loop
        // if one exists, otherwise there's nothing navigable at all.
        if let Some(last) = self.active_loops().last() {
            self.selected_loop_id = Some(last.id.clone());
            self.refresh_loops_selection();
        }
    }

    fn cross_backward_from_backlog(&mut self) {
        if self.try_cross_to_loops_last() {
            return;
        }
        self.cross_backward_from_loops();
    }

    fn try_cross_to_loops_last(&mut self) -> bool {
        let Some(id) = self.active_loops().last().map(|lp| lp.id.clone()) else {
            return false;
        };
        self.projects_panel_focus = ProjectsPanelFocus::Loops;
        self.selected_loop_id = Some(id);
        self.refresh_loops_selection();
        true
    }

    fn try_cross_to_backlog_last(&mut self) -> bool {
        if self.backlog_specs.is_empty() {
            return false;
        }
        self.projects_panel_focus = ProjectsPanelFocus::Backlog;
        self.selected_backlog = self.backlog_specs.len() - 1;
        true
    }

    /// Focuses `History` on its last finished loop. When collapsed there is
    /// nothing to select — focus lands on the header without disturbing
    /// `selected_loop_id`, matching `try_cross_to_history_first`.
    fn try_cross_to_history_last(&mut self) -> bool {
        let finished_ids: Vec<String> = self
            .finished_loops()
            .iter()
            .map(|lp| lp.id.clone())
            .collect();
        if finished_ids.is_empty() {
            return false;
        }
        self.projects_panel_focus = ProjectsPanelFocus::History;
        if !self.history_collapsed {
            self.selected_history = finished_ids.len() - 1;
            self.selected_loop_id = Some(finished_ids[finished_ids.len() - 1].clone());
            self.refresh_loops_selection();
        }
        true
    }

    fn navigate_active_loops_prev(&mut self) {
        let visible_ids: Vec<String> = self
            .active_loops()
            .into_iter()
            .map(|lp| lp.id.clone())
            .collect();
        if visible_ids.is_empty() {
            self.cross_backward_from_loops();
            return;
        }
        let current = self
            .selected_loop_id
            .as_ref()
            .and_then(|id| visible_ids.iter().position(|vid| vid == id))
            .unwrap_or(0);
        if current > 0 {
            self.selected_loop_id = Some(visible_ids[current - 1].clone());
            self.refresh_loops_selection();
            return;
        }
        self.cross_backward_from_loops();
    }

    fn navigate_backlog_prev(&mut self) {
        if self.backlog_specs.is_empty() {
            self.cross_backward_from_backlog();
            return;
        }
        if self.selected_backlog > 0 {
            self.selected_backlog -= 1;
            return;
        }
        self.cross_backward_from_backlog();
    }

    fn navigate_history_prev(&mut self) {
        let finished_ids: Vec<String> = self
            .finished_loops()
            .iter()
            .map(|lp| lp.id.clone())
            .collect();
        if finished_ids.is_empty() || self.history_collapsed {
            if self.try_cross_to_backlog_last() {
                return;
            }
            self.cross_backward_from_loops();
            return;
        }
        let current = self
            .selected_loop_id
            .as_ref()
            .and_then(|id| finished_ids.iter().position(|vid| vid == id))
            .unwrap_or(0);
        if current > 0 {
            self.selected_history = current - 1;
            self.selected_loop_id = Some(finished_ids[current - 1].clone());
            self.refresh_loops_selection();
            return;
        }
        if self.try_cross_to_backlog_last() {
            return;
        }
        self.cross_backward_from_loops();
    }

    fn navigate_from_rag_info_prev(&mut self) {
        if self.try_cross_to_history_last() {
            return;
        }
        if self.try_cross_to_backlog_last() {
            return;
        }
        if self.try_cross_to_loops_last() {
            return;
        }
        if self.projects.is_empty() {
            return;
        }
        self.projects_panel_focus = ProjectsPanelFocus::Projects;
        self.selected_project = self.projects.len() - 1;
    }

    fn select_prev_agent_panel(&mut self) {
        if !self.rag_info.has_rag_activity() {
            self.retreat_agent_selection();
            return;
        }

        if self.agents_rag_focused {
            self.agents_rag_focused = false;
            if !self.agents.is_empty() {
                let prev = self.selected;
                self.selected = self.agents.len() - 1;
                self.update_agent_section_focus_on_change(prev);
            }
            self.reset_log_scroll();
            return;
        }

        if self.agents.is_empty() || self.selected == 0 {
            self.agents_rag_focused = true;
            self.reset_log_scroll();
            return;
        }

        self.retreat_agent_selection();
    }

    fn retreat_agent_selection(&mut self) {
        if self.agents.is_empty() {
            return;
        }

        let prev = self.selected;
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.agents.len() - 1);
        self.update_agent_section_focus_on_change(prev);
        self.reset_log_scroll();
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
        Ok(())
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

    /// Completed/failed loops for the sidebar's `History` section.
    pub fn finished_loops(&self) -> Vec<&crate::domain::loops::Loop> {
        self.loops
            .iter()
            .filter(|lp| matches!(lp.status, LoopStatus::Completed | LoopStatus::Failed))
            .collect()
    }

    fn refresh_loops_selection(&mut self) {
        let visible = self.visible_loops();
        if visible.is_empty() {
            self.selected_loop_id = None;
            self.loop_details = None;
            self.loop_runs.clear();
            self.loop_selected_spec = 0;
            self.loop_selected_node = 0;
            self.loop_live_state = None;
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
            if self.projects_panel_focus == ProjectsPanelFocus::RagInfo {
                self.projects_panel_focus = ProjectsPanelFocus::Projects;
            }
            self.agents_rag_focused = false;
        }

        self.rag_file_status = self.db.rag_per_file_status().unwrap_or_default();

        Ok(())
    }

    fn rag_vector_search(
        &self,
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
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| anyhow::anyhow!("No tokio runtime"))?;
        rt.block_on(async {
            let store = crate::rag::vector_store::VectorStore::new(dimensions).await?;
            let embedder = crate::rag::embedding_client::client_from_config(&config)?;
            let query_vec = embedder.embed(query)?;
            store.search_similar(&query_vec, top_k).await
        })
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

    pub fn selected_loop_details(&self) -> Option<&crate::domain::loops::LoopDetails> {
        self.loop_details.as_ref()
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

    pub fn selected_filtered_knowledge_index(&self) -> Option<usize> {
        self.filtered_knowledge_indices()
            .iter()
            .position(|&idx| idx == self.selected_knowledge)
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

    #[allow(dead_code)]
    pub fn visible_projects_panels(&self) -> Vec<ProjectsPanelFocus> {
        let mut panels = vec![
            ProjectsPanelFocus::Projects,
            ProjectsPanelFocus::Loops,
            ProjectsPanelFocus::Backlog,
            ProjectsPanelFocus::History,
            ProjectsPanelFocus::Knowledge,
        ];
        if self.rag_info.has_rag_activity() {
            panels.push(ProjectsPanelFocus::RagInfo);
        }
        panels
    }

    fn project_panel_has_navigable_items(&self, panel: ProjectsPanelFocus) -> bool {
        match panel {
            ProjectsPanelFocus::Projects => !self.projects.is_empty(),
            ProjectsPanelFocus::Loops => !self.active_loops().is_empty(),
            ProjectsPanelFocus::Backlog => !self.backlog_specs.is_empty(),
            ProjectsPanelFocus::History => !self.finished_loops().is_empty(),
            ProjectsPanelFocus::Knowledge => true,
            ProjectsPanelFocus::RagInfo => self.rag_info.has_rag_activity(),
        }
    }

    fn normalize_projects_panel_focus(&mut self) {
        if self.project_panel_has_navigable_items(self.projects_panel_focus) {
            return;
        }

        let fallback = self
            .visible_projects_panels()
            .into_iter()
            .find(|panel| self.project_panel_has_navigable_items(*panel));
        if let Some(panel) = fallback {
            self.projects_panel_focus = panel;
        }
    }

    pub(crate) fn focus_projects_panel_from_edge(&mut self, from_top: bool) {
        let panels = self.visible_projects_panels();
        let ordered = if from_top {
            panels
        } else {
            panels.into_iter().rev().collect::<Vec<_>>()
        };

        let selected = ordered
            .iter()
            .copied()
            .find(|panel| self.project_panel_has_navigable_items(*panel))
            .or_else(|| ordered.first().copied())
            .unwrap_or(ProjectsPanelFocus::Projects);
        self.projects_panel_focus = selected;
    }

    #[allow(dead_code)]
    pub fn cycle_projects_panel_focus(&mut self, forward: bool) {
        let panels = self.visible_projects_panels();
        let current = panels
            .iter()
            .position(|panel| *panel == self.projects_panel_focus)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % panels.len()
        } else {
            current.checked_sub(1).unwrap_or(panels.len() - 1)
        };
        self.projects_panel_focus = panels[next];
        self.log_scroll = 0;
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

    pub fn toggle_sidebar_mode(&mut self) {
        self.sidebar_mode = match self.sidebar_mode {
            SidebarMode::Agents => SidebarMode::Projects,
            SidebarMode::Projects => SidebarMode::Agents,
        };
        if self.sidebar_mode == SidebarMode::Projects {
            self.normalize_projects_panel_focus();
        }
        self.agents_rag_focused = false;
        self.reset_log_scroll();
    }

    /// Toggles the sidebar's `History` section between its collapsed header
    /// (just the count) and the expanded finished-loop list. On expand,
    /// selects the first finished loop so the main panel previews it — same
    /// as entering the section from an edge.
    pub fn toggle_history_collapsed(&mut self) {
        self.history_collapsed = !self.history_collapsed;
        if self.history_collapsed {
            return;
        }
        let Some(first) = self.finished_loops().first().map(|lp| lp.id.clone()) else {
            return;
        };
        self.selected_history = 0;
        self.selected_loop_id = Some(first);
        self.refresh_loops_selection();
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

    pub fn cycle_loop_node(&mut self, forward: bool) {
        let Some(spec) = self.selected_loop_spec() else {
            return;
        };
        if spec.nodes.is_empty() {
            return;
        }
        self.loop_selected_node = if forward {
            (self.loop_selected_node + 1) % spec.nodes.len()
        } else {
            self.loop_selected_node
                .checked_sub(1)
                .unwrap_or(spec.nodes.len() - 1)
        };
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
        if self.sidebar_mode == SidebarMode::Projects {
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
        if self.sidebar_mode != SidebarMode::Projects {
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
        if self.sidebar_mode != SidebarMode::Projects {
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
                            "Fresh-session fallback also failed for '{}': {e2}; marking orphaned",
                            session.name
                        );
                        // Neither the resume nor the fresh-launch attempt could
                        // start this CLI (binary missing, no resume flag and the
                        // original args no longer work, etc.) — leaving the row
                        // 'active' would strand it invisibly forever. Orphan it
                        // so the TUI's orphaned-sessions view can revive or
                        // dismiss it, preserving its workdir and CLI.
                        let _ = self.db.mark_session_orphaned(&session.id);
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
        self.interactive_agents.push(agent);
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
            crate::tui::ui::ACCENT,
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
                tracing::warn!(
                    "Skipping auto-resume of session '{}': old process (pid {:?}) is still alive (same boot)",
                    session.name,
                    session.pid
                );
                // Per-session orphan marking — only this session is stranded
                // if we crash right here, not every remaining active session.
                let _ = self.db.mark_session_orphaned(&session.id);
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
    use super::{build_resumed_session_args, process_is_alive, should_resume_session};
    use crate::db::session::InteractiveSession;
    use crate::db::Database;
    use crate::tui::app::types::App;
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
            active_run_pool_id: None,
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
    fn finished_loops_only_includes_completed_and_failed() {
        use crate::domain::loops::LoopStatus;

        let db = test_db();
        db.insert_loop(&make_loop("l-running", "Running Loop", LoopStatus::Running))
            .unwrap();
        db.insert_loop(&make_loop("l-done", "Done Loop", LoopStatus::Completed))
            .unwrap();
        db.insert_loop(&make_loop("l-failed", "Failed Loop", LoopStatus::Failed))
            .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");

        let mut ids: Vec<&str> = app
            .finished_loops()
            .iter()
            .map(|lp| lp.id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["l-done", "l-failed"]);
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
    fn toggle_history_collapsed_selects_first_finished_loop() {
        use crate::domain::loops::LoopStatus;

        let db = test_db();
        // A second, active loop so the initially-selected loop (whichever
        // `refresh_loops_selection` defaults to) isn't already "l-done" —
        // otherwise the assertion below can't tell a real selection change
        // from a coincidence.
        db.insert_loop(&make_loop("l-active", "Active Loop", LoopStatus::Running))
            .unwrap();
        db.insert_loop(&make_loop("l-done", "Done Loop", LoopStatus::Completed))
            .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.selected_loop_id = Some("l-active".to_string());
        assert!(app.history_collapsed, "history starts collapsed");

        app.toggle_history_collapsed();
        assert!(!app.history_collapsed);
        assert_eq!(app.selected_loop_id.as_deref(), Some("l-done"));
        assert_eq!(app.selected_history, 0);

        app.toggle_history_collapsed();
        assert!(app.history_collapsed);
    }

    #[test]
    fn select_next_cycles_projects_loops_backlog_history_then_wraps() {
        use crate::domain::loops::LoopStatus;
        use crate::tui::app::types::{ProjectsPanelFocus, SidebarMode};

        let db = test_db();
        db.upsert_project(&make_project("hash0", "/tmp/proj0"))
            .unwrap();
        db.insert_loop(&make_loop("l-active", "Active Loop", LoopStatus::Running))
            .unwrap();
        db.insert_loop(&make_loop("l-done", "Done Loop", LoopStatus::Completed))
            .unwrap();
        db.insert_loop_spec(&make_backlog_spec("spec-a", "Spec A", Some("/tmp/proj0")))
            .unwrap();

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.toggle_sidebar_mode();
        assert!(matches!(app.sidebar_mode, SidebarMode::Projects));
        app.projects_panel_focus = ProjectsPanelFocus::Projects;
        app.selected_project = 0;

        app.select_next();
        assert_eq!(app.projects_panel_focus, ProjectsPanelFocus::Loops);
        assert_eq!(app.selected_loop_id.as_deref(), Some("l-active"));

        app.select_next();
        assert_eq!(app.projects_panel_focus, ProjectsPanelFocus::Backlog);
        assert_eq!(app.selected_backlog, 0);

        app.select_next();
        assert_eq!(app.projects_panel_focus, ProjectsPanelFocus::History);
        assert!(
            app.history_collapsed,
            "crossing into History shouldn't auto-expand it"
        );

        // Collapsed History has nothing to cycle through, so the next arrow
        // press falls through to the end of the chain and wraps back to
        // Projects (there's no RAG activity in this test).
        app.select_next();
        assert_eq!(app.projects_panel_focus, ProjectsPanelFocus::Projects);
        assert_eq!(app.selected_project, 0);

        // And the reverse chain mirrors it exactly.
        app.select_prev();
        assert_eq!(app.projects_panel_focus, ProjectsPanelFocus::History);

        app.select_prev();
        assert_eq!(app.projects_panel_focus, ProjectsPanelFocus::Backlog);

        app.select_prev();
        assert_eq!(app.projects_panel_focus, ProjectsPanelFocus::Loops);
        assert_eq!(app.selected_loop_id.as_deref(), Some("l-active"));
    }
}
