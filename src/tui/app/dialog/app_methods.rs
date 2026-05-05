use super::super::types::AgentEntry;
use super::super::types::App;
use super::new_agent::{BackgroundTrigger, NewAgentDialog, NewTaskType};
use super::prompt::SimplePromptDialog;
use crate::application::ports::AgentRepository;
use crate::domain::models::Trigger;
use anyhow::Result;
use std::path::Path;

impl App {
    pub fn open_edit_dialog(&mut self) {
        let prev_focus = self.focus;
        let Some(agent) = self.agents.get(self.selected) else {
            return;
        };
        let agent_dir = match agent {
            AgentEntry::Agent(a) => a.working_dir.as_deref(),
            _ => None,
        };
        let AgentEntry::Agent(a) = agent else {
            return; // editing not supported for Interactive/Terminal/Group
        };
        let mut dialog = NewAgentDialog::new(agent_dir);
        dialog.prev_focus = Some(prev_focus);
        populate_dialog_from_agent(&mut dialog, a);
        dialog.refresh_model_suggestions();
        self.new_agent_dialog = Some(dialog);
        self.focus = super::super::types::Focus::NewAgentDialog;
    }

    pub fn open_new_agent_dialog(&mut self) {
        let prev_focus = self.focus;

        // Get working dir from current agent if available
        let agent_dir = self.selected_agent().and_then(|entry| match entry {
            AgentEntry::Interactive(idx) => self
                .interactive_agents
                .get(*idx)
                .map(|a| a.working_dir.as_str()),
            AgentEntry::Terminal(idx) => self
                .terminal_agents
                .get(*idx)
                .map(|a| a.working_dir.as_str()),
            _ => None,
        });

        self.new_agent_dialog = Some(NewAgentDialog::new(agent_dir));
        self.new_agent_dialog.as_mut().unwrap().prev_focus = Some(prev_focus);
        self.focus = super::super::types::Focus::NewAgentDialog;
    }

    pub fn close_new_agent_dialog(&mut self) {
        if let Some(dialog) = &self.new_agent_dialog {
            if let Some(prev) = dialog.prev_focus {
                self.focus = prev;
            } else {
                self.focus = super::super::types::Focus::Home;
            }
        } else {
            self.focus = super::super::types::Focus::Home;
        }
        self.new_agent_dialog = None;
    }

    /// Open prompt template dialog with the specified template and optional initial content.
    /// Restores any persisted session for the current workdir.
    /// Injects an invisible system block on the first prompt per workdir (idempotent).
    pub fn open_simple_prompt_dialog(
        &mut self,
        initial_content: Option<std::collections::HashMap<String, String>>,
    ) {
        let prev_focus = self.focus;
        let workdir = self.current_workdir();
        let mut dialog = SimplePromptDialog::new();

        // Restore persisted session for this workdir if available
        let has_persisted = self.prompt_builder_sessions.contains_key(&workdir);
        if has_persisted {
            if let Some(session) = self.prompt_builder_sessions.get(&workdir) {
                session.restore_into(&mut dialog);
            }
        }

        // Determine system block idempotency: send on first prompt or on solo-mode transition
        let is_solo = !self.sync_available();
        let state = self
            .workdir_system_state
            .get(&workdir)
            .cloned()
            .unwrap_or_default();
        let should_send_system = !state.sent || (!state.sent_as_solo && is_solo);
        if should_send_system {
            dialog.system_content = Some(self.build_system_content(is_solo));
        }

        if let Some(content) = initial_content {
            for (section_name, section_content) in content {
                if section_name == "instruction" || section_name.starts_with("instruction_") {
                    let instr_id = dialog
                        .enabled_sections
                        .iter()
                        .find(|s| *s == "instruction" || s.starts_with("instruction_"))
                        .cloned()
                        .unwrap_or_else(|| "instruction_1".to_string());
                    let char_len = section_content.chars().count();
                    dialog.sections.insert(instr_id.clone(), section_content);
                    dialog.section_cursors.insert(instr_id, char_len);
                } else if section_name == "context" || section_name.starts_with("context_") {
                    // Context sections from initial_content (e.g. context transfer) are locked
                    let ctx_id = dialog.add_section_with_content(&section_name, section_content);
                    dialog.lock_section(&ctx_id);
                } else {
                    dialog.add_section_with_content(&section_name, section_content);
                }
            }
            dialog.focused_section = 0;
        }
        dialog.prev_focus = Some(prev_focus);
        self.simple_prompt_dialog = Some(dialog);
        self.focus = super::super::types::Focus::PromptTemplateDialog;
    }

    /// Build system block content for the invisible system prompt section.
    fn build_system_content(&self, is_solo: bool) -> String {
        let mut lines: Vec<String> = Vec::new();

        if let Some(state) = self.sync_panel_state() {
            lines.push(format!(
                "workspace: {} | agents: {} | vibe: {}",
                state.workdir,
                state.participant_count,
                state.vibe.as_str()
            ));
            if !is_solo {
                if !state.active_intents.is_empty() {
                    lines.push("active missions:".to_string());
                    for intent in &state.active_intents {
                        lines.push(format!(
                            "  - {} [{}] {}: {}",
                            intent.agent_name,
                            intent.impact.as_str(),
                            intent.mission,
                            intent.description
                        ));
                    }
                }
                let chatter: Vec<_> = state
                    .recent_messages
                    .iter()
                    .filter(|m| m.kind.is_chatter())
                    .take(5)
                    .collect();
                if !chatter.is_empty() {
                    lines.push("recent messages:".to_string());
                    for msg in chatter {
                        lines.push(format!("  - {}: {}", msg.agent_name, msg.message));
                    }
                }
            }
        } else {
            let workdir = self.current_workdir();
            lines.push(format!("workspace: {}", workdir.display()));
        }

        if is_solo {
            lines.push(String::new());
            lines.push(
                "You are in solo mode in Canopy Sync. \
                No other agents are currently active in this workspace."
                    .to_string(),
            );
        }

        lines.push(String::new());
        lines.push("You are operating within the Canopy multi-agent framework.".to_string());
        lines.push(
            "- Follow skill:mindset principles for all actions \
            (verify before reporting, think critically, relentless resourcefulness, \
            security guard, token efficiency)."
                .to_string(),
        );
        lines.push(
            "- When taking actions that affect shared workspace state, use \
            canopy_sync_broadcast and canopy_sync_declare_intent tools to coordinate \
            with peer agents."
                .to_string(),
        );
        lines.push(
            "- Report execution status with canopy_agent_report when working on \
            scheduled tasks."
                .to_string(),
        );

        lines.join("\n")
    }

    /// Build a compact sync context string from active intents and recent chatter.
    /// Kept for backwards compatibility; not used by the prompt builder any more.
    #[allow(dead_code)]
    fn build_sync_context_text(&self) -> Option<String> {
        let state = self.sync_panel_state()?;
        let mut lines = Vec::new();

        lines.push(format!(
            "workspace: {} | agents: {} | vibe: {}",
            state.workdir,
            state.participant_count,
            state.vibe.as_str()
        ));

        if !state.active_intents.is_empty() {
            lines.push("active missions:".to_string());
            for intent in &state.active_intents {
                lines.push(format!(
                    "  - {} [{}] {}: {}",
                    intent.agent_name,
                    intent.impact.as_str(),
                    intent.mission,
                    intent.description
                ));
            }
        }

        let chatter: Vec<_> = state
            .recent_messages
            .iter()
            .filter(|m| m.kind.is_chatter())
            .take(5)
            .collect();
        if !chatter.is_empty() {
            lines.push("recent messages:".to_string());
            for msg in chatter {
                lines.push(format!("  - {}: {}", msg.agent_name, msg.message));
            }
        }

        Some(lines.join("\n"))
    }

    /// Close simple prompt dialog and persist its state for the current workdir.
    pub fn close_simple_prompt_dialog(&mut self) {
        self._close_simple_prompt_dialog(true);
    }

    /// Close simple prompt dialog without persisting its state (e.g. after sending).
    pub fn discard_simple_prompt_dialog(&mut self) {
        self._close_simple_prompt_dialog(false);
    }

    fn _close_simple_prompt_dialog(&mut self, persist: bool) {
        if let Some(dialog) = self.simple_prompt_dialog.take() {
            if let Some(prev) = dialog.prev_focus {
                self.focus = prev;
            } else {
                self.focus = super::super::types::Focus::Agent;
            }
            if persist {
                let workdir = self.current_workdir();
                let session = super::prompt::PromptBuilderSession::from_dialog(&dialog);
                self.prompt_builder_sessions.insert(workdir, session);
            }
        } else {
            self.focus = super::super::types::Focus::Agent;
        }
    }

    pub fn launch_new_agent(&mut self) -> Result<()> {
        // Take dialog out of self to avoid borrow conflicts
        let Some(dialog) = self.new_agent_dialog.take() else {
            return Ok(());
        };

        let model = if dialog.model.is_empty() {
            None
        } else {
            Some(dialog.model.clone())
        };

        let _was_interactive = matches!(
            dialog.task_type,
            NewTaskType::Interactive | NewTaskType::Terminal
        );
        let prev_focus = dialog.prev_focus;

        if let Some(ref edit_id) = dialog.edit_id {
            // ── Edit mode: partial-update existing agent ──────────────────
            let model_ref = model.as_deref();
            match dialog.task_type {
                NewTaskType::Background => match dialog.background_trigger {
                    BackgroundTrigger::Cron => {
                        self.update_scheduled(&dialog, model_ref, edit_id)?;
                    }
                    BackgroundTrigger::Watch => {
                        self.update_watcher_edit(&dialog, model_ref, edit_id)?;
                    }
                },
                NewTaskType::Interactive | NewTaskType::Terminal => {}
            }
            self.new_agent_dialog = None;
            self.refresh_agents()?;
            self.focus = prev_focus.unwrap_or(super::super::types::Focus::Preview);
            return Ok(());
        }

        // ── Create mode ───────────────────────────────────────────────────
        // Track the name of the newly created agent to select it after refresh
        let new_agent_name = match dialog.task_type {
            NewTaskType::Interactive => {
                self.launch_interactive(&dialog)?;
                self.interactive_agents
                    .last()
                    .map(|agent| agent.name.clone())
            }
            NewTaskType::Background => {
                match dialog.background_trigger {
                    BackgroundTrigger::Cron => {
                        self.launch_scheduled(&dialog, model)?;
                    }
                    BackgroundTrigger::Watch => {
                        self.launch_watcher(&dialog, model)?;
                    }
                }
                None
            }
            NewTaskType::Terminal => {
                self.launch_terminal(&dialog)?;
                self.terminal_agents.last().map(|agent| agent.name.clone())
            }
        };

        self.new_agent_dialog = None;

        self.refresh_agents()?;

        // Select the newly created agent specifically instead of just the last agent
        if let Some(agent_name) = new_agent_name {
            if let Some(position) = self
                .agents
                .iter()
                .position(|entry| entry.id(self) == agent_name)
            {
                self.selected = position;
            }
        }

        // All new sessions start in focus mode
        self.focus = super::super::types::Focus::Agent;
        Ok(())
    }

    fn update_scheduled(
        &self,
        dialog: &NewAgentDialog,
        model: Option<&str>,
        id: &str,
    ) -> Result<()> {
        if dialog.prompt.is_empty() {
            return Ok(());
        }
        let Some(mut agent) = self.db.get_agent(id)? else {
            return Ok(());
        };
        agent.prompt = dialog.prompt.clone();
        if let Some(Trigger::Cron { schedule_expr }) = &mut agent.trigger {
            *schedule_expr = dialog.cron_expr.clone();
        }
        agent.cli = dialog.selected_cli();
        agent.model = model.map(String::from);
        agent.working_dir = if dialog.working_dir.is_empty() {
            None
        } else {
            Some(dialog.working_dir.clone())
        };
        self.db.upsert_agent(&agent)?;
        Ok(())
    }

    fn update_watcher_edit(
        &self,
        dialog: &NewAgentDialog,
        model: Option<&str>,
        id: &str,
    ) -> Result<()> {
        if dialog.prompt.is_empty() || dialog.watch_path.is_empty() {
            return Ok(());
        }
        let Some(mut agent) = self.db.get_agent(id)? else {
            return Ok(());
        };
        agent.prompt = dialog.prompt.clone();
        agent.cli = dialog.selected_cli();
        agent.model = model.map(String::from);
        if let Some(Trigger::Watch { path, events, .. }) = &mut agent.trigger {
            *path = dialog.watch_path.clone();
            *events = crate::domain::models::WatchEvent::parse_list(&dialog.watch_events)
                .unwrap_or_default();
        }
        self.db.upsert_agent(&agent)?;
        Ok(())
    }

    fn launch_interactive(&mut self, dialog: &NewAgentDialog) -> Result<()> {
        use crate::tui::agent::InteractiveAgent;
        let cli = dialog.selected_cli();
        self.record_cli_usage(cli.as_str());
        let dir = dialog.working_dir.clone();
        // Append yolo flag to args when yolo mode is enabled
        let base_args = dialog.selected_args();
        let args = if dialog.yolo_mode {
            if let Some(ref flag) = dialog.selected_yolo_flag() {
                Some(match base_args {
                    Some(ref a) => format!("{a} {flag}"),
                    None => flag.clone(),
                })
            } else {
                base_args
            }
        } else {
            base_args
        };
        let fallback = dialog.selected_fallback_args();
        let accent = dialog.selected_accent_color();
        let model = if dialog.model.is_empty() {
            None
        } else {
            Some(dialog.model.clone())
        };
        let model_flag = dialog
            .cli_configs
            .get(dialog.cli_index)
            .and_then(|c| c.as_ref())
            .and_then(|c| c.model_flag.clone());
        let (cols, rows) = pty_dimensions(self.last_panel_inner);
        let existing_refs: Vec<&str> = self
            .interactive_agents
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        let agent = InteractiveAgent::spawn(
            cli,
            &dir,
            cols,
            rows,
            args.as_deref(),
            fallback.as_deref(),
            accent,
            None,
            &existing_refs,
            model.as_deref(),
            model_flag.as_deref(),
        )?;
        // Persist session in registry
        let _ = self.db.insert_interactive_session(
            &agent.id,
            &agent.name,
            agent.cli.as_str(),
            &dir,
            args.as_deref(),
        );
        let _ = self.db.register_project_path(Path::new(&dir));
        self.interactive_agents.push(agent);
        self.whimsg
            .notify_event(crate::tui::whimsg::WhimContext::AgentSpawned);
        Ok(())
    }

    fn launch_scheduled(&mut self, dialog: &NewAgentDialog, model: Option<String>) -> Result<()> {
        use chrono::Utc;
        if dialog.prompt.is_empty() {
            return Ok(());
        }
        let cli = dialog.selected_cli();
        let id = new_short_id("agent");
        let working_dir = if dialog.working_dir.is_empty() {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "/".to_string())
        } else {
            dialog.working_dir.clone()
        };
        let log_path = agent_log_path(&id);
        let agent = crate::domain::models::Agent {
            id,
            prompt: dialog.prompt.clone(),
            trigger: Some(crate::domain::models::Trigger::Cron {
                schedule_expr: dialog.cron_expr.clone(),
            }),
            cli,
            model,
            working_dir: Some(working_dir),
            enabled: true,
            created_at: Utc::now(),
            log_path,
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        };
        self.db.upsert_agent(&agent)?;
        if let Some(workdir) = agent.working_dir.as_deref() {
            let _ = self.db.register_project_path(Path::new(workdir));
        }
        Ok(())
    }

    fn launch_watcher(&mut self, dialog: &NewAgentDialog, model: Option<String>) -> Result<()> {
        use chrono::Utc;
        if dialog.prompt.is_empty() || dialog.watch_path.is_empty() {
            return Ok(());
        }
        let cli = dialog.selected_cli();
        let id = new_short_id("watch");
        let events: Vec<_> = dialog
            .watch_events
            .iter()
            .filter_map(|e| crate::domain::models::WatchEvent::from_str(e))
            .collect();
        if events.is_empty() {
            return Ok(());
        }
        let log_path = agent_log_path(&id);
        let agent = crate::domain::models::Agent {
            id,
            prompt: dialog.prompt.clone(),
            trigger: Some(crate::domain::models::Trigger::Watch {
                path: dialog.watch_path.clone(),
                events,
                debounce_seconds: 5,
                recursive: false,
            }),
            cli,
            model,
            working_dir: None,
            enabled: true,
            created_at: Utc::now(),
            log_path,
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        };
        self.db.upsert_agent(&agent)?;
        Ok(())
    }

    pub(super) fn launch_terminal(&mut self, dialog: &NewAgentDialog) -> Result<()> {
        use crate::tui::agent::InteractiveAgent;

        let shell = dialog.selected_shell();
        let dir = dialog.working_dir.clone();
        let (cols, rows) = pty_dimensions(self.last_panel_inner);
        let existing_refs: Vec<&str> = self
            .terminal_agents
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        let agent = InteractiveAgent::spawn_terminal(
            shell,
            &dir,
            cols,
            rows,
            None,
            &existing_refs,
            crate::tui::ui::ACCENT,
        )?;
        let _ = self
            .db
            .insert_terminal_session(&agent.id, &agent.name, shell, &dir);
        let _ = self.db.register_project_path(Path::new(&dir));
        // Load command history into cache
        let hist = crate::tui::terminal_history::load_history(&self.data_dir, &agent.name);
        self.terminal_histories.insert(agent.name.clone(), hist);
        self.terminal_agents.push(agent);
        self.whimsg
            .notify_event(crate::tui::whimsg::WhimContext::AgentSpawned);
        Ok(())
    }
}

// ── Free helpers ──────────────────────────────────────────────────

fn new_short_id(prefix: &str) -> String {
    format!("{}-{}", prefix, &uuid::Uuid::new_v4().to_string()[..8])
}

fn agent_log_path(id: &str) -> String {
    dirs::home_dir()
        .map(|h| h.join(".canopy/logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/canopy/logs"))
        .join(id)
        .with_extension("log")
        .to_string_lossy()
        .to_string()
}

/// Populate a `NewAgentDialog` from an existing agent's fields.
fn populate_dialog_from_agent(dialog: &mut NewAgentDialog, a: &crate::domain::models::Agent) {
    dialog.edit_id = Some(a.id.clone());
    dialog.task_type = NewTaskType::Background;
    dialog.prompt = a.prompt.clone();
    dialog.model = a.model.clone().unwrap_or_default();
    dialog.working_dir = a.working_dir.clone().unwrap_or_default();
    dialog.field = 2;

    if let Some(idx) = dialog
        .available_clis
        .iter()
        .position(|c| c.as_str() == a.cli.as_str())
    {
        dialog.cli_index = idx;
    }

    match &a.trigger {
        Some(crate::domain::models::Trigger::Cron { schedule_expr }) => {
            dialog.background_trigger = BackgroundTrigger::Cron;
            dialog.cron_expr = schedule_expr.clone();
        }
        Some(crate::domain::models::Trigger::Watch { path, events, .. }) => {
            dialog.background_trigger = BackgroundTrigger::Watch;
            dialog.watch_path = path.clone();
            dialog.watch_events = events
                .iter()
                .map(|e| e.to_string().to_lowercase())
                .collect();
        }
        None => {
            dialog.background_trigger = BackgroundTrigger::Cron;
        }
    }
}

/// Resolve PTY dimensions from the last known panel size, falling back to terminal size.
fn pty_dimensions(last_panel_inner: (u16, u16)) -> (u16, u16) {
    if last_panel_inner != (0, 0) {
        return last_panel_inner;
    }
    let (tw, th) = ratatui::crossterm::terminal::size().unwrap_or((120, 40));
    (tw.saturating_sub(28), th.saturating_sub(4))
}
