use super::types::{AgentEntry, App, Focus};
use crate::tui::agent::{AgentStatus, InteractiveAgent};
use std::time::Duration;

const SHADOW_SUMMARY_LINGER_SECS: u64 = 7;
const SHADOW_SUMMARY_INSTRUCTION: &str = "Session terminated by user. Before exit, call intelligence_upsert with kind='session' and persist a concise summary including: mission outcome, key decisions, pending follow-ups, and any reusable facts/patterns.";

/// Strip ANSI escape sequences from a string for plain-text display.
fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip CSI sequences: ESC [ ... final_byte
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() || next == 'm' {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn recent_output_snippet(agent: &InteractiveAgent, n: usize) -> String {
    let clean: Vec<String> = agent
        .last_output_lines(n)
        .into_iter()
        .map(|line| strip_ansi_codes(&line))
        .filter(|line| !line.is_empty())
        .collect();

    if clean.is_empty() {
        String::new()
    } else {
        format!("\n{}", clean.join("\n"))
    }
}

#[derive(Clone, Copy)]
enum SessionTarget {
    Interactive(usize),
    Terminal(usize),
}

fn reverse_sorted_indices(mut indices: Vec<usize>) -> Vec<usize> {
    indices.sort_unstable();
    indices.reverse();
    indices
}

fn poll_agents(agents: &mut [InteractiveAgent]) {
    for agent in agents {
        agent.poll();
    }
}

fn remove_session_name(sessions: &mut Vec<InteractiveAgent>, idx: usize) -> Option<String> {
    let agent_name = sessions.get(idx).map(|agent| agent.name.clone())?;

    sessions.remove(idx);
    Some(agent_name)
}

fn log_terminal_exit(name: &str, shell: &str, code: i32, output_snippet: &str) {
    tracing::warn!(
        "Terminal '{}' ({}) exited with code {code}.{}",
        name,
        shell,
        if output_snippet.is_empty() {
            ""
        } else {
            output_snippet
        }
    );
}

impl App {
    pub fn notify_mouse_move(&mut self) {
        if let Some(ref mut brain) = self.home_brain {
            brain.notify_mouse();
        }
        if let Some(ref mut brain) = self.sidebar_brain {
            brain.notify_mouse();
        }
    }

    pub fn tick_banner_animation(&mut self) {
        if let Some(ref mut brain) = self.home_brain {
            brain.step();
        }

        if self.focus != Focus::Home {
            return;
        }

        let (cols, rows) = effective_brain_dims(self.last_panel_inner);
        if cols < 6 || rows < 3 {
            return;
        }

        if brain_needs_reinit(&self.home_brain, rows, cols) {
            self.home_brain = Some(make_brain(rows, cols, 80));
        }
    }

    pub fn ensure_sidebar_brain(&mut self) {
        let (_tw, th) = ratatui::crossterm::terminal::size().unwrap_or((120, 40));
        let sidebar_h = th.saturating_sub(2);
        let cols = (30u16.saturating_sub(2)) as usize;
        let dashboard_h = if sidebar_h >= 6 { 6 } else { 0 };
        let rows = sidebar_h.saturating_sub(dashboard_h) as usize;

        if cols < 6 || rows < 3 {
            return;
        }

        if brain_needs_reinit(&self.sidebar_brain, rows, cols) {
            self.sidebar_brain = Some(make_brain(rows, cols, 60));
        }

        if let Some(ref mut brain) = self.sidebar_brain {
            brain.step();
        }
    }

    pub fn dismiss_brain(&mut self) {
        if let Some(ref mut brain) = self.home_brain {
            *brain = super::super::brians_brain::BriansBrain::new(brain.rows, brain.cols, 80);
        }
    }

    pub(super) fn dismiss_copied(&mut self) {
        if self.show_copied && self.copied_at.elapsed() > std::time::Duration::from_secs(2) {
            self.show_copied = false;
        }
    }

    fn focusable_agent_indices(&self) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                matches!(
                    entry,
                    AgentEntry::Interactive(_) | AgentEntry::Terminal(_) | AgentEntry::Group(_)
                )
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn move_interactive_selection(&mut self, forward: bool) {
        let focusable = self.focusable_agent_indices();
        if focusable.is_empty() {
            return;
        }

        let current_pos = focusable
            .iter()
            .position(|&idx| idx == self.selected)
            .unwrap_or(0);

        let next_pos = if forward {
            (current_pos + 1) % focusable.len()
        } else {
            current_pos.checked_sub(1).unwrap_or(focusable.len() - 1)
        };

        self.selected = focusable[next_pos];
        self.focus = Focus::Agent;
        self.activate_selected_entry();
    }

    fn activate_split_group(&mut self, idx: usize) {
        let Some(group) = self.split_groups.get(idx) else {
            self.active_split_id = None;
            return;
        };

        self.active_split_id = Some(group.id.clone());
        self.split_right_focused = false;
    }

    fn activate_interactive_session(&mut self, idx: usize) {
        self.active_split_id = None;

        let Some(agent) = self.interactive_agents.get(idx) else {
            return;
        };

        agent.mark_viewed();
    }

    fn activate_terminal_session(&mut self, idx: usize) {
        self.active_split_id = None;

        let Some(agent) = self.terminal_agents.get(idx) else {
            return;
        };

        agent.mark_viewed();
    }

    fn active_split_sessions(&self) -> Option<(String, String)> {
        let split_id = self.active_split_id.as_ref()?;
        let group = self
            .split_groups
            .iter()
            .find(|group| group.id == *split_id)?;

        Some((group.session_a.clone(), group.session_b.clone()))
    }

    fn selected_session_target(&self) -> Option<SessionTarget> {
        let selected = self.selected_agent()?;
        match selected {
            AgentEntry::Interactive(idx) => Some(SessionTarget::Interactive(*idx)),
            AgentEntry::Terminal(idx) => Some(SessionTarget::Terminal(*idx)),
            _ => None,
        }
    }

    fn session_target_by_name(&self, name: &str) -> Option<SessionTarget> {
        if let Some(idx) = self
            .interactive_agents
            .iter()
            .position(|agent| agent.name == name)
        {
            return Some(SessionTarget::Interactive(idx));
        }

        let idx = self
            .terminal_agents
            .iter()
            .position(|agent| agent.name == name)?;
        Some(SessionTarget::Terminal(idx))
    }

    fn close_session_target(&mut self, target: SessionTarget, exit_code: i32) -> bool {
        match target {
            SessionTarget::Interactive(idx) => self.close_interactive_session_at(idx, exit_code),
            SessionTarget::Terminal(idx) => self.close_terminal_session_at(idx),
        }
    }

    fn remove_session_target(&mut self, target: SessionTarget) -> bool {
        match target {
            SessionTarget::Interactive(idx) => self.remove_interactive_session_entry(idx),
            SessionTarget::Terminal(idx) => self.remove_terminal_session_entry(idx),
        }
    }

    fn delete_group_at(&mut self, idx: usize) -> bool {
        let Some(group_id) = self.split_groups.get(idx).map(|group| group.id.clone()) else {
            return false;
        };

        self.delete_group_by_id(&group_id)
    }

    fn delete_group_by_id(&mut self, id: &str) -> bool {
        if !self.split_groups.iter().any(|group| group.id == id) {
            return false;
        }

        let _ = self.db.delete_group(id);
        self.split_groups.retain(|group| group.id != id);
        if self.active_split_id.as_deref() == Some(id) {
            self.active_split_id = None;
        }
        true
    }

    fn mark_selected_terminal_viewed(&mut self) {
        if !matches!(self.focus, Focus::Agent | Focus::Preview) {
            return;
        }

        let Some(AgentEntry::Terminal(idx)) = self.agents.get(self.selected) else {
            return;
        };
        let Some(agent) = self.terminal_agents.get(*idx) else {
            return;
        };

        agent.mark_viewed();
    }

    fn mark_selected_interactive_viewed(&mut self) {
        if !matches!(self.focus, Focus::Agent | Focus::Preview) {
            return;
        }

        let Some(AgentEntry::Interactive(idx)) = self.agents.get(self.selected) else {
            return;
        };
        let Some(agent) = self.interactive_agents.get(*idx) else {
            return;
        };

        agent.mark_viewed();
    }

    fn exited_terminal_indices(&self) -> Vec<usize> {
        self.terminal_agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| matches!(agent.status, AgentStatus::Exited(_)))
            .map(|(idx, _)| idx)
            .collect()
    }

    fn handle_terminal_exit(&mut self, idx: usize) {
        let Some(agent) = self.terminal_agents.get(idx) else {
            return;
        };
        if agent.exit_notified {
            return;
        }

        let AgentStatus::Exited(code) = agent.status else {
            return;
        };

        let agent_id = agent.id.clone();
        let agent_name = agent.name.clone();
        let shell = agent.shell.clone();
        let output_snippet = recent_output_snippet(agent, 5);

        let _ = self.db.finish_terminal_session(&agent_id);
        if code != 0 {
            log_terminal_exit(&agent_name, &shell, code, &output_snippet);
        }

        let Some(agent) = self.terminal_agents.get_mut(idx) else {
            return;
        };
        agent.exit_notified = true;
    }

    fn exited_interactive_indices(&self) -> Vec<usize> {
        self.interactive_agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| matches!(agent.status, AgentStatus::Exited(_)))
            .map(|(idx, _)| idx)
            .collect()
    }

    fn notify_failed_interactive_exit(
        &mut self,
        agent_id: &str,
        cli: &str,
        code: i32,
        output_snippet: &str,
    ) {
        tracing::warn!(
            "Agent '{agent_id}' ({cli}) exited with code {code}.{}",
            if output_snippet.is_empty() {
                ""
            } else {
                output_snippet
            }
        );

        self.whimsg
            .notify_event(crate::tui::whimsg::WhimContext::AgentFailed);
        if !self.notifications_enabled {
            return;
        }

        let output = output_snippet.trim_start_matches('\n').to_string();
        self.notification_service
            .notify_agent_failed(agent_id, cli, code, &output);
    }

    fn handle_interactive_exit(&mut self, idx: usize) {
        let Some(agent) = self.interactive_agents.get(idx) else {
            return;
        };
        if agent.exit_notified {
            return;
        }

        let AgentStatus::Exited(code) = agent.status else {
            return;
        };

        let agent_id = agent.id.clone();
        let agent_name = agent.name.clone();
        let working_dir = agent.working_dir.clone();
        let cli = agent.cli.as_str().to_string();
        let output_snippet = recent_output_snippet(agent, 5);

        // Finalize nursery if this was a seed creation session
        if let Some(ref nursery_path) = self.nursery_path {
            if nursery_path.to_string_lossy() == working_dir && code == 0 {
                match crate::domain::nursery::finalize_nursery(nursery_path) {
                    Ok(seed_id) => {
                        // Bind the session to the new seed
                        let _ = self.db.bind_session_to_seed(&agent_id, &seed_id);
                    }
                    Err(e) => {
                        // Log the error — nursery cleanup is non-fatal
                        eprintln!("Nursery finalization failed: {e}");
                    }
                }
                self.nursery_path = None;
            } else if code != 0 {
                // Clean up temp dir on error exit
                let _ = std::fs::remove_dir_all(nursery_path);
                self.nursery_path = None;
            }
        }

        let _ = self.db.finish_interactive_session(&agent_id, code);
        let _ = self
            .db
            .close_agent_missions(&agent_id, &agent_name, &working_dir);
        if code != 0 {
            self.notify_failed_interactive_exit(&agent_id, &cli, code, &output_snippet);
        }

        let Some(agent) = self.interactive_agents.get_mut(idx) else {
            return;
        };
        agent.exit_notified = true;
    }

    fn successful_interactive_exit_indices(&self) -> Vec<usize> {
        self.interactive_agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| matches!(agent.status, AgentStatus::Exited(0)))
            .map(|(idx, _)| idx)
            .collect()
    }

    fn remove_terminal_sessions(&mut self, indices: Vec<usize>) {
        for idx in reverse_sorted_indices(indices) {
            let _ = self.remove_session_target(SessionTarget::Terminal(idx));
        }
    }

    fn remove_interactive_sessions(&mut self, indices: Vec<usize>) {
        for idx in reverse_sorted_indices(indices) {
            let _ = self.remove_session_target(SessionTarget::Interactive(idx));
        }
    }

    fn terminate_active_split_session(&mut self) -> bool {
        let Some(split_id) = self.active_split_id.clone() else {
            return false;
        };
        let Some(target) = self
            .split_groups
            .iter()
            .find(|group| group.id == split_id)
            .map(|group| {
                if self.split_right_focused {
                    group.session_b.clone()
                } else {
                    group.session_a.clone()
                }
            })
        else {
            return false;
        };

        self.kill_session_by_name(&target);
        self.delete_group_by_id(&split_id)
    }

    fn terminate_selected_session(&mut self) -> bool {
        let Some(target) = self.selected_session_target() else {
            return false;
        };

        self.close_session_target(target, 0)
    }

    fn sync_selection_after_session_mutation(&mut self) {
        if self.agents.is_empty() {
            self.selected = 0;
            return;
        }

        if self.selected >= self.agents.len() {
            self.selected = self.agents.len() - 1;
        }
    }

    fn reset_focus_after_session_mutation(&mut self) {
        if self.focus == Focus::Agent
            || matches!(self.focus, Focus::ContextTransfer | Focus::RagTransfer)
        {
            self.focus = Focus::Preview;
        }
    }

    pub fn next_interactive(&mut self) {
        self.move_interactive_selection(true);
    }

    pub fn prev_interactive(&mut self) {
        self.move_interactive_selection(false);
    }

    /// Activate split or clear it based on the currently selected entry.
    fn activate_selected_entry(&mut self) {
        let Some(entry) = self.agents.get(self.selected) else {
            self.active_split_id = None;
            return;
        };

        match entry {
            AgentEntry::Group(idx) => self.activate_split_group(*idx),
            AgentEntry::Interactive(idx) => self.activate_interactive_session(*idx),
            AgentEntry::Terminal(idx) => self.activate_terminal_session(*idx),
            _ => {
                self.active_split_id = None;
            }
        }
    }

    pub(super) fn resize_interactive_agents(&mut self) {
        let (cols, rows) = self.last_panel_inner;
        if cols == 0 || rows == 0 {
            return;
        }

        // In split mode, only resize the two sessions participating in the split.
        // Other sessions keep their last size to avoid unnecessary resize churn.
        let split_sessions = self.active_split_sessions();

        for agent in &mut self.interactive_agents {
            let dominated = split_sessions
                .as_ref()
                .is_some_and(|(a, b)| agent.name != *a && agent.name != *b);
            if dominated {
                continue;
            }
            if agent.last_pty_cols != cols || agent.last_pty_rows != rows {
                agent.resize(cols, rows);
            }
        }
        for agent in &mut self.terminal_agents {
            let dominated = split_sessions
                .as_ref()
                .is_some_and(|(a, b)| agent.name != *a && agent.name != *b);
            if dominated {
                continue;
            }
            // Warp-mode terminals lose 3 rows for the input box
            let effective_rows = if agent.warp_mode {
                rows.saturating_sub(3)
            } else {
                rows
            };
            if agent.last_pty_cols != cols || agent.last_pty_rows != effective_rows {
                agent.resize(cols, effective_rows);
            }
        }
    }

    /// Poll terminal agent processes for exit status.
    pub(super) fn poll_terminal_agents(&mut self) {
        self.mark_selected_terminal_viewed();
        poll_agents(&mut self.terminal_agents);

        let exited_indices = self.exited_terminal_indices();
        if exited_indices.is_empty() {
            return;
        }

        for &idx in &exited_indices {
            self.handle_terminal_exit(idx);
        }
        self.remove_terminal_sessions(exited_indices);
        self.finish_session_mutation();
    }

    pub(super) fn poll_interactive_agents(&mut self) {
        self.mark_selected_interactive_viewed();
        poll_agents(&mut self.interactive_agents);

        let exited_indices = self.exited_interactive_indices();
        for &idx in &exited_indices {
            self.handle_interactive_exit(idx);
        }

        let removed_indices = self.successful_interactive_exit_indices();
        if removed_indices.is_empty() {
            return;
        }

        self.remove_interactive_sessions(removed_indices);
        self.finish_session_mutation();
    }

    pub fn rerun_selected(&self) -> anyhow::Result<()> {
        let Some(agent) = self.agents.get(self.selected) else {
            return Ok(());
        };
        match agent {
            AgentEntry::Interactive(_) | AgentEntry::Terminal(_) | AgentEntry::Group(_) => Ok(()),
            _ => {
                use crate::application::ports::StateRepository;
                let port = self
                    .db
                    .get_state("port")?
                    .unwrap_or_else(|| "7755".to_string());
                super::send_mcp_task_run(&port, agent.id(self))
            }
        }
    }

    #[allow(dead_code)]
    pub fn kill_selected_agent(&mut self) {
        let Some(AgentEntry::Interactive(idx)) = self.agents.get(self.selected) else {
            return;
        };
        if self.close_interactive_session_at(*idx, 0) {
            self.finish_session_mutation();
        }
    }

    pub fn delete_selected(&mut self) -> anyhow::Result<()> {
        let Some(selected) = self.selected_agent() else {
            return Ok(());
        };

        match selected {
            AgentEntry::Agent(agent) => {
                use crate::application::ports::AgentRepository;
                self.db.delete_agent(&agent.id)?;
            }
            AgentEntry::Group(idx) => {
                if !self.delete_group_at(*idx) {
                    return Ok(());
                }
            }
            AgentEntry::Interactive(idx) => {
                if !self.close_session_target(SessionTarget::Interactive(*idx), 0) {
                    return Ok(());
                }
            }
            AgentEntry::Terminal(idx) => {
                if !self.close_session_target(SessionTarget::Terminal(*idx), 0) {
                    return Ok(());
                }
            }
        }

        self.finish_session_mutation();
        Ok(())
    }

    /// Dissolve all split groups that contain the given session name.
    fn dissolve_groups_for_session(&mut self, session_name: &str) {
        let ids_to_dissolve: Vec<String> = self
            .split_groups
            .iter()
            .filter(|group| group.session_a == session_name || group.session_b == session_name)
            .map(|group| group.id.clone())
            .collect();

        for id in ids_to_dissolve {
            let _ = self.delete_group_by_id(&id);
        }
    }

    pub fn cleanup(&mut self) {
        for agent in &mut self.interactive_agents {
            agent.kill();
        }
        for agent in &mut self.terminal_agents {
            agent.kill();
        }
        // Clear any lingering toast notifications from the Windows Action Center
        crate::domain::notification::clear_notifications_on_exit();
    }

    /// Terminate the session(s) currently in focus.
    ///
    /// - Single agent/terminal: kill it and remove.
    /// - Active split group: kill both sessions and dissolve the group.
    pub fn terminate_focused_session(&mut self) {
        let terminated = if self.active_split_id.is_some() {
            self.terminate_active_split_session()
        } else {
            self.terminate_selected_session()
        };
        if !terminated {
            return;
        }

        self.finish_session_mutation();
    }

    /// Kill and remove a session by name (interactive or terminal).
    fn kill_session_by_name(&mut self, name: &str) {
        let Some(target) = self.session_target_by_name(name) else {
            return;
        };

        let _ = self.close_session_target(target, 0);
    }

    fn finish_session_mutation(&mut self) {
        let _ = self.refresh_agents();
        self.sync_selection_after_session_mutation();
        self.reset_focus_after_session_mutation();
    }

    fn close_interactive_session_at(&mut self, idx: usize, exit_code: i32) -> bool {
        let Some(agent_id) = self
            .interactive_agents
            .get(idx)
            .map(|agent| agent.id.clone())
        else {
            return false;
        };

        let _ = self.db.finish_interactive_session(&agent_id, exit_code);
        let Some(agent) = self.interactive_agents.get(idx) else {
            return false;
        };
        agent.schedule_shadow_shutdown(
            SHADOW_SUMMARY_INSTRUCTION,
            Duration::from_secs(SHADOW_SUMMARY_LINGER_SECS),
        );
        self.remove_session_target(SessionTarget::Interactive(idx))
    }

    fn close_terminal_session_at(&mut self, idx: usize) -> bool {
        let Some(agent_id) = self.terminal_agents.get(idx).map(|agent| agent.id.clone()) else {
            return false;
        };

        let _ = self.db.finish_terminal_session(&agent_id);
        let Some(agent) = self.terminal_agents.get_mut(idx) else {
            return false;
        };
        agent.kill();
        self.remove_session_target(SessionTarget::Terminal(idx))
    }

    fn remove_interactive_session_entry(&mut self, idx: usize) -> bool {
        let Some(agent_name) = remove_session_name(&mut self.interactive_agents, idx) else {
            return false;
        };

        self.dissolve_groups_for_session(&agent_name);
        true
    }

    fn remove_terminal_session_entry(&mut self, idx: usize) -> bool {
        let Some(agent_name) = remove_session_name(&mut self.terminal_agents, idx) else {
            return false;
        };

        self.dissolve_groups_for_session(&agent_name);
        true
    }
}

// ── Brain helpers ─────────────────────────────────────────────────

fn make_brain(rows: usize, cols: usize, density: u64) -> super::super::brians_brain::BriansBrain {
    let mut brain = super::super::brians_brain::BriansBrain::new(rows, cols, density);
    brain.last_step =
        std::time::Instant::now() - std::time::Duration::from_millis(brain.step_interval_ms);
    brain
}

fn brain_needs_reinit(
    brain: &Option<super::super::brians_brain::BriansBrain>,
    rows: usize,
    cols: usize,
) -> bool {
    brain
        .as_ref()
        .is_none_or(|b| b.rows != rows || b.cols != cols)
}

/// Resolve effective brain dimensions from panel size, falling back to terminal size.
fn effective_brain_dims(panel: (u16, u16)) -> (usize, usize) {
    let (pw, ph) = panel;
    if pw >= 6 && ph >= 3 {
        return (pw as usize, ph as usize);
    }
    let (tw, th) = ratatui::crossterm::terminal::size().unwrap_or((120, 40));
    let cols = (tw / 2).saturating_sub(2) as usize;
    let rows = th.saturating_sub(3) as usize;
    (cols, rows)
}
