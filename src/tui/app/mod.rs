mod agents;
mod data;
pub mod dialog;
mod sync;

use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
use crate::tui::prompt_templates::PromptTemplates;

pub(crate) use data::send_mcp_task_run;

// ── Types ───────────────────────────────────────────────────────

pub mod session_resume;
pub mod terminal_search;
pub mod types;
pub mod utils;

pub(crate) use session_resume::build_resumed_session_args;
pub use terminal_search::TerminalSearch;
pub(crate) use types::ContextTransferSource;
use types::RagTransferModal;
pub use types::{AgentEntry, App, Focus, ProjectsPanelFocus, SidebarMode};

impl App {
    pub fn new(db: Arc<Database>, data_dir: &Path) -> Result<Self> {
        let home = dirs::home_dir().unwrap_or_default();
        let canopy_dir = home.join(".canopy");
        let canopy_config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);

        let (system_info_tx, system_info_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let initial = crate::system::SystemInfo::new();
            let _ = system_info_tx.send(initial);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let mut info = crate::system::SystemInfo::default();
                info.update();
                let _ = system_info_tx.send(info);
            }
        });

        let mut app = Self {
            db,
            data_dir: data_dir.to_path_buf(),
            agents: Vec::new(),
            active_runs: HashMap::new(),
            recent_runs: Vec::new(),
            interactive_agents: Vec::new(),
            terminal_agents: Vec::new(),
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
            quit_confirm: false,
            sidebar_brain: None,
            home_brain: None,
            sidebar_click_map: Vec::new(),
            projects: Vec::new(),
            selected_project: 0,
            projects_panel_focus: ProjectsPanelFocus::Projects,
            global_rag_queue: Vec::new(),
            selected_rag_queue: 0,
            rag_info: crate::db::project::RagInfoSummary::default(),
            sidebar_visible: true,
            sync_panel_visible: true,
            term_width: 0,
            show_legend: false,
            show_copied: false,
            copied_at: std::time::Instant::now() - std::time::Duration::from_secs(10),
            last_scroll_at: std::time::Instant::now() - std::time::Duration::from_secs(999),
            last_panel_inner: (0, 0),
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
            system_info_rx,
            last_system_update: std::time::Instant::now() - std::time::Duration::from_secs(10),
            process_start_time: std::time::Instant::now(),
            cli_usage: {
                let mut usage = dirs::home_dir()
                    .map(|h| crate::domain::usage_stats::CliUsage::load(&h.join(".canopy")))
                    .unwrap_or_default();
                if usage.ensure_first_run() {
                    let _ = dirs::home_dir()
                        .and_then(|h| usage.save(&h.join(".canopy")).ok().map(|_| ()));
                }
                usage
            },
            playground_active: false,
            playground_query: String::new(),
            playground_results: Vec::new(),
            playground_selected: 0,
            playground_last_search: std::time::Instant::now(),
            playground_last_executed_query: String::new(),
            playground_detail_mode: false,
            playground_scroll: 0,
            playground_project_hash: None,
            rag_paused: false,
            agents_rag_focused: false,
            sync_scroll_offset: 0,
            last_sync_area: None,
            workdir_system_state: HashMap::new(),
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
        self.refresh_rag_state()?;
        self.refresh_active_runs()?;
        self.poll_interactive_agents();
        self.poll_terminal_agents();
        self.tick_banner_animation();
        self.ensure_sidebar_brain();
        self.refresh_log();
        self.auto_hide_sidebar();
        self.dismiss_copied();
        self.update_whimsg_context();
        self.resize_interactive_agents();
        self.refresh_playground_search()?;

        // Non-blocking check for updated system info from background thread
        while let Ok(info) = self.system_info_rx.try_recv() {
            self.system_info = info;
            self.last_system_update = std::time::Instant::now();
        }

        Ok(())
    }

    /// Perform debounced RAG search in playground mode
    fn refresh_playground_search(&mut self) -> Result<()> {
        if !self.playground_active {
            return Ok(());
        }

        // Only search if query changed and debounce time has passed (300ms)
        let since_last = self.playground_last_search.elapsed().as_millis();
        if since_last < 300 {
            return Ok(());
        }

        let query = self.playground_query.trim().to_string();
        if query.is_empty() {
            self.playground_results.clear();
            self.playground_selected = 0;
            self.playground_last_executed_query.clear();
            return Ok(());
        }

        if self.playground_last_executed_query == query {
            return Ok(());
        }

        if let Ok(results) =
            self.db
                .search_chunks(&query, self.playground_project_hash.as_deref(), 50)
        {
            self.playground_results = results;
            self.playground_selected = 0;
        }
        self.playground_last_executed_query = query;
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
        if self.projects_panel_focus != ProjectsPanelFocus::Projects || self.projects.is_empty() {
            return;
        }

        self.selected_project = (self.selected_project + 1) % self.projects.len();
        self.reset_log_scroll();
    }

    fn select_next_agent_panel(&mut self) {
        if !self.rag_info.has_rag_activity() {
            self.advance_agent_selection();
            return;
        }

        if self.agents_rag_focused {
            self.agents_rag_focused = false;
            if !self.agents.is_empty() {
                self.selected = 0;
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

        self.selected = (self.selected + 1) % self.agents.len();
        self.reset_log_scroll();
    }

    pub fn select_prev(&mut self) {
        if self.sidebar_mode == SidebarMode::Projects {
            self.select_prev_project_panel();
            return;
        }

        self.select_prev_agent_panel();
    }

    fn select_prev_project_panel(&mut self) {
        if self.projects_panel_focus != ProjectsPanelFocus::Projects || self.projects.is_empty() {
            return;
        }

        self.selected_project = self
            .selected_project
            .checked_sub(1)
            .unwrap_or(self.projects.len() - 1);
        self.reset_log_scroll();
    }

    fn select_prev_agent_panel(&mut self) {
        if !self.rag_info.has_rag_activity() {
            self.retreat_agent_selection();
            return;
        }

        if self.agents_rag_focused {
            self.agents_rag_focused = false;
            if !self.agents.is_empty() {
                self.selected = self.agents.len() - 1;
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

        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.agents.len() - 1);
        self.reset_log_scroll();
    }

    fn reset_log_scroll(&mut self) {
        self.log_scroll = 0;
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
        Ok(())
    }

    fn refresh_rag_state(&mut self) -> Result<()> {
        self.global_rag_queue = self.db.list_rag_queue(50)?;
        self.rag_info = self.db.rag_info_summary()?;
        self.rag_paused = self
            .db
            .get_state("rag_paused")?
            .map(|v| v == "1")
            .unwrap_or(false);

        if self.global_rag_queue.is_empty() {
            self.selected_rag_queue = 0;
        } else {
            self.selected_rag_queue = self
                .selected_rag_queue
                .min(self.global_rag_queue.len().saturating_sub(1));
        }

        // If RagInfo becomes unavailable (no chunks), reset focus away from it.
        if !self.rag_info.has_rag_activity() {
            if self.projects_panel_focus == ProjectsPanelFocus::RagInfo {
                self.projects_panel_focus = ProjectsPanelFocus::Projects;
            }
            self.agents_rag_focused = false;
        }

        Ok(())
    }

    pub fn selected_agent(&self) -> Option<&AgentEntry> {
        self.agents.get(self.selected)
    }

    pub fn selected_project(&self) -> Option<&crate::domain::project::Project> {
        self.projects.get(self.selected_project)
    }

    pub fn delete_selected_project(&mut self) -> Result<()> {
        let Some(hash) = self.selected_project().map(|p| p.hash.clone()) else {
            return Ok(());
        };
        self.db.delete_project(&hash)?;
        self.refresh_projects()?;
        self.refresh_rag_state()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn visible_projects_panels(&self) -> Vec<ProjectsPanelFocus> {
        let mut panels = vec![ProjectsPanelFocus::Projects];
        if self.rag_info.has_rag_activity() {
            panels.push(ProjectsPanelFocus::RagInfo);
        }
        panels
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
        self.agents_rag_focused = false;
        self.reset_log_scroll();
    }

    pub fn selected_playground_chunk(&self) -> Option<&crate::db::project::Chunk> {
        self.playground_results.get(self.playground_selected)
    }

    pub fn toggle_sync_panel(&mut self) {
        self.sync_panel_visible = !self.sync_panel_visible;
    }

    fn live_agent_for_entry(&self, entry: &AgentEntry) -> Option<&InteractiveAgent> {
        match entry {
            AgentEntry::Interactive(idx) => self.interactive_agents.get(*idx),
            AgentEntry::Terminal(idx) => self.terminal_agents.get(*idx),
            AgentEntry::Agent(_) | AgentEntry::Group(_) => None,
        }
    }

    fn selected_live_agent(&self) -> Option<&InteractiveAgent> {
        self.selected_agent()
            .and_then(|entry| self.live_agent_for_entry(entry))
    }

    /// Return the working directory of the currently selected agent,
    /// or the parent of the data directory as a fallback.
    pub fn current_workdir(&self) -> PathBuf {
        self.selected_live_agent()
            .map(|agent| PathBuf::from(&agent.working_dir))
            .unwrap_or_else(|| {
                self.data_dir
                    .parent()
                    .unwrap_or(&self.data_dir)
                    .to_path_buf()
            })
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
        for run in &self.recent_runs {
            let Some(finished) = run.finished_at else {
                continue;
            };
            if (now - finished).num_seconds() >= 60 {
                continue;
            }
            match run.status {
                crate::domain::models::RunStatus::Error
                | crate::domain::models::RunStatus::Timeout => {
                    self.whimsg.notify_event(WhimContext::AgentFailed);
                }
                crate::domain::models::RunStatus::Success => {
                    self.whimsg.notify_event(WhimContext::AgentDone);
                }
                _ => {}
            }
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
            .map(|agent| agent.last_lines(50))
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
        if let Some(entry_pos) = self
            .agents
            .iter()
            .position(|entry| matches!(entry, AgentEntry::Interactive(idx) if *idx == dest_ia_idx))
        {
            self.selected = entry_pos;
        }
        self.focus = Focus::Agent;
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
            AgentEntry::Agent(_) | AgentEntry::Group(_) => None,
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
            ContextTransferSource::Interactive(idx) => {
                let agent = self.context_transfer_agent(source)?;
                let capture_kind = interactive_capture_kind(agent);
                let max_units = Self::interactive_capture_units(agent, capture_kind);
                let initial_units = initial_capture_units(max_units, &self.context_transfer_config);
                Some(ContextTransferModal::new(idx, capture_kind, initial_units))
            }
            ContextTransferSource::Terminal(idx) => {
                self.context_transfer_agent(source)?;
                let initial_units = initial_capture_units(20, &self.context_transfer_config);
                Some(ContextTransferModal::new_terminal(idx, initial_units))
            }
        }
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
            "kind: rag_chunk\nquery: {}\npath: {}\nchunk_index: {}\nlanguage: {}\ncontent:\n{}",
            query, chunk.source_path, chunk.chunk_index, chunk.lang, chunk.content
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
    ) {
        let cli = crate::domain::models::Cli::from_str(&session.cli);
        let cli_config = canopy_config.get_cli(cli.as_str());
        let args = build_resumed_session_args(
            session,
            cli_config.and_then(|config| config.interactive_args.as_deref()),
            cli_config.and_then(|config| config.resume_args.as_deref()),
            cli_config.and_then(|config| config.session_resume_cmd.as_deref()),
            cli_config.and_then(|config| config.yolo_flag.as_deref()),
        );
        let existing_ids = self.interactive_agent_names();

        let agent = match InteractiveAgent::spawn(
            cli.clone(),
            &session.working_dir,
            cols,
            rows,
            args.as_deref(),
            cli_config.and_then(|config| config.fallback_interactive_args.as_deref()),
            Self::resume_session_accent(cli_config),
            Some(&session.name),
            &existing_ids,
            None,
            cli_config.and_then(|config| config.model_flag.as_deref()),
        ) {
            Ok(agent) => agent,
            Err(e) => {
                tracing::warn!("Failed to auto-resume session '{}': {e}", session.name);
                return;
            }
        };

        let _ = self.db.insert_interactive_session(
            &agent.id,
            &agent.name,
            cli.as_str(),
            &session.working_dir,
            args.as_deref(),
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
        self.terminal_histories.insert(agent.name.clone(), hist);
        self.terminal_agents.push(agent);
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
        let _ = self.db.mark_orphaned_sessions();

        let home = dirs::home_dir().unwrap_or_default();
        let canopy_config = crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy"));
        let (cols, rows) = Self::session_panel_size();

        for session in &sessions {
            self.resume_interactive_session(session, &canopy_config, cols, rows);
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
    use super::build_resumed_session_args;
    use crate::db::session::InteractiveSession;

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
}
