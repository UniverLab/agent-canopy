use super::types::{AgentEntry, App, Focus};
use crate::tui::agent::{AgentStatus, InteractiveAgent};
use crate::tui::terminal_history::save_history;
use regex::Regex;
use std::sync::LazyLock;
use std::time::Duration;

const SHADOW_SUMMARY_LINGER_SECS: u64 = 7;
const SHADOW_SUMMARY_INSTRUCTION: &str = "Session terminated by user. Before exit, call intelligence_upsert with kind='session' and persist a concise summary including: mission outcome, key decisions, pending follow-ups, and any reusable facts/patterns.";

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("invalid ANSI regex"));

/// Strip ANSI escape sequences from a string for plain-text display.
fn strip_ansi_codes(s: &str) -> String {
    ANSI_RE.replace_all(s, "").into_owned()
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

/// Selection index after a session-list mutation: keep the previously-selected
/// entry by identity when it survived the mutation, otherwise clamp into range.
/// Pure so the "navigate Down over a reaped session" fix is unit-testable.
fn selection_after_mutation(
    entry_ids: &[&str],
    anchor: Option<&str>,
    prev_selected: usize,
) -> usize {
    if entry_ids.is_empty() {
        return 0;
    }
    if let Some(anchor) = anchor {
        if let Some(pos) = entry_ids.iter().position(|id| *id == anchor) {
            return pos;
        }
    }
    prev_selected.min(entry_ids.len() - 1)
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

    /// Update atmosphere mouse context (call from event loop on MouseMove).
    pub fn notify_atmosphere_mouse(&mut self, col: u16, row: u16) {
        let (prev_col, prev_row) = self.atmosphere_last_mouse;
        let delta_col = col as i16 - prev_col as i16;
        let delta_row = row as i16 - prev_row as i16;
        self.atmosphere_ctx.mouse_col = col;
        self.atmosphere_ctx.mouse_row = row;
        self.atmosphere_ctx.mouse_delta_col = delta_col;
        self.atmosphere_ctx.mouse_delta_row = delta_row;
        self.atmosphere_last_mouse = (col, row);
        self.atmosphere.notify_mouse(col, row, delta_col, delta_row);
    }

    /// Advance the atmosphere engine one tick (called from App::refresh).
    pub(super) fn tick_atmosphere(&mut self) {
        use chrono::Timelike;
        self.atmosphere_ctx.hour = chrono::Local::now().hour() as u8;
        let scroll_active = self.last_scroll_at.elapsed().as_secs_f32() < 1.0;
        self.atmosphere_ctx.scroll_velocity = if scroll_active { 0.8 } else { 0.0 };
        self.atmosphere_ctx.typing_speed = if self.animation_tick.is_multiple_of(3) {
            0.1
        } else {
            0.0
        };

        if self.atmosphere_ctx.firefly_caught {
            self.queue_mission_event(crate::tui::gamification::MissionEvent::FireflyCaught);
            self.atmosphere_ctx.firefly_caught = false;
        }
        self.atmosphere_ctx.mouse_clicked = false;
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

    /// Indices of all agent entries the user can navigate to from focus mode:
    /// interactive + terminal sessions, groups, and background agents. The
    /// RAG-info panel is reached by walking past either end of this list, not
    /// by a dedicated index. Order matches `app.agents`, which itself is
    /// `[background..., interactive..., terminal..., groups...]`.
    fn focusable_agent_indices(&self) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                matches!(
                    entry,
                    AgentEntry::Interactive(_)
                        | AgentEntry::Terminal(_)
                        | AgentEntry::Group(_)
                        | AgentEntry::Agent(_)
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

        let next_pos = crate::tui::selection::move_index(current_pos, focusable.len(), forward);

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
        if let Some(ref nursery_path_ref) = self.nursery_path {
            // Clone the path to avoid borrow conflict when we clear self.nursery_path
            let nursery_path = nursery_path_ref.clone();
            if nursery_path.to_string_lossy() == working_dir && code == 0 {
                match crate::domain::nursery::finalize_nursery(&nursery_path) {
                    Ok(seed_id) => {
                        // Bind the session to the new seed
                        let _ = self.db.bind_session_to_seed(&agent_id, &seed_id);
                        self.queue_mission_event(
                            crate::tui::gamification::MissionEvent::FirstSeedCreated,
                        );
                    }
                    Err(e) => {
                        tracing::error!("Nursery finalization failed: {e}");
                        self.notification_service.notify_nursery_failed(&e);
                    }
                }
            } else if code != 0 {
                // Clean up temp dir on error exit
                let _ = std::fs::remove_dir_all(&nursery_path);
            }
            // Remove nursery session record from DB — it should not persist as a normal session
            let _ = self.db.remove_interactive_session(&agent_id);
            // Also remove the nursery project entry if it was registered
            let _ = self.db.unregister_project_path(&nursery_path);
            // Clear nursery state — do this after all uses of nursery_path
            self.nursery_path = None;
        } else {
            let _ = self.db.finish_interactive_session(&agent_id, code);
        }

        if let Some(ref sb) = self.active_sandbox {
            let sandbox = sb.clone();
            let merge_result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async { crate::domain::sandbox::merge_sandbox(&sandbox).await })
            });
            match merge_result {
                Ok(outcome) => {
                    let status = match &outcome {
                        crate::domain::sandbox::MergeOutcome::CleanMerge => "merged",
                        crate::domain::sandbox::MergeOutcome::ConflictResolution => "merged",
                        crate::domain::sandbox::MergeOutcome::MergeFailed(_) => "failed",
                    };
                    let _ = self.db.update_sandbox_run_status(&sandbox.id, status);
                    if let crate::domain::sandbox::MergeOutcome::MergeFailed(ref reason) = outcome {
                        tracing::error!("Sandbox merge failed: {reason}");
                        self.notification_service
                            .notify_nursery_failed(&format!("Sandbox merge failed: {reason}"));
                    }
                }
                Err(e) => {
                    tracing::error!("Sandbox merge error: {e}");
                    let _ = self.db.update_sandbox_run_status(&sandbox.id, "failed");
                }
            }
            self.active_sandbox = None;
        }

        let _ = self
            .db
            .close_agent_missions(&agent_id, &agent_name, &working_dir);
        if code != 0 {
            self.notify_failed_interactive_exit(&agent_id, &cli, code, &output_snippet);
        } else if self.session_args_contain_yolo(&agent_id, &cli) {
            self.queue_mission_event(crate::tui::gamification::MissionEvent::YoloTaskCompleted);
        }

        let Some(agent) = self.interactive_agents.get_mut(idx) else {
            return;
        };
        agent.exit_notified = true;
    }

    fn successful_interactive_exit_indices(&self) -> Vec<usize> {
        // Keep a successfully-finished session on screen while it is the one the
        // user is currently viewing — its final output often carries useful
        // end-of-run stats. It is reaped on a later poll, once the user moves
        // the selection to another session.
        let viewing = match self.selected_session_target() {
            Some(SessionTarget::Interactive(idx)) => Some(idx),
            _ => None,
        };
        self.interactive_agents
            .iter()
            .enumerate()
            .filter(|(idx, agent)| {
                matches!(agent.status, AgentStatus::Exited(0)) && Some(*idx) != viewing
            })
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

        let anchor = self.selected_entry_identity();
        self.remove_interactive_sessions(removed_indices);
        self.finish_session_mutation_preserving(anchor.as_deref());
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
            AgentEntry::Corrupt(corrupt) => {
                // Deletes by id without parsing the stored row — a corrupt
                // row must always be removable.
                use crate::application::ports::AgentRepository;
                self.db.delete_agent(&corrupt.id)?;
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
            AgentEntry::Orphaned(idx) => {
                // Orphaned sessions are DB-only; just drop the entry.
                self.orphaned_sessions.remove(*idx);
                self.finish_session_mutation();
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

    fn selected_entry_identity(&self) -> Option<String> {
        self.agents
            .get(self.selected)
            .map(|entry| entry.id(self).to_string())
    }

    fn finish_session_mutation_preserving(&mut self, anchor: Option<&str>) {
        let _ = self.refresh_agents();
        let ids: Vec<String> = self
            .agents
            .iter()
            .map(|entry| entry.id(self).to_string())
            .collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.selected = selection_after_mutation(&id_refs, anchor, self.selected);
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

        // Design decision: keep scrollback as plain text and exclude
        // full-screen programs, rather than switch to storing a replayable
        // raw ANSI byte stream.
        //
        // A byte stream would let a full-screen app (vim, htop, a nested
        // canopy) replay faithfully, but it changes what lands on disk in
        // ways this spec explicitly flags as needing its own size/safety
        // review, and it would still have to solve accumulation (the same
        // append-forever bug that corrupts the current text format).
        // Excluding full-screen output instead reuses the existing
        // plain-text, line-capped format and needs no new on-disk shape.
        //
        // vt100 already tells us when a full-screen program is active:
        // `Screen::alternate_screen()` mirrors DECSET 1049, and the crate
        // never threads the alternate grid into scrollback (it's allocated
        // with `scrollback_len: 0` — see vt100::Screen::new). So if the
        // session is still in alternate-screen mode at close time,
        // `last_lines` would only be able to return the program's current
        // on-screen frame (there's no line history to fall back to), and
        // persisting that as scrollback text is exactly the "rendered
        // frames printed back as literal text" bug. Skip the capture
        // entirely in that case and keep whatever was already persisted —
        // once the program exits back to the shell, a later close captures
        // real shell history again, since alternate-grid content never
        // reached the primary grid's scrollback in the first place.
        if !agent.in_alternate_screen() {
            let scrollback = agent.last_lines(2000);
            if let Some(hist) = self.terminal_histories.get_mut(&agent.name) {
                let lines: Vec<String> = scrollback.lines().map(|s| s.to_string()).collect();
                hist.update_scrollback(&lines);
                save_history(&self.data_dir, &agent.name, hist);
            }
        }
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

    pub(crate) fn selected_session_is_exited(&self) -> bool {
        match self.selected_session_target() {
            Some(SessionTarget::Interactive(idx)) => self
                .interactive_agents
                .get(idx)
                .is_some_and(|agent| matches!(agent.status, AgentStatus::Exited(_))),
            Some(SessionTarget::Terminal(idx)) => self
                .terminal_agents
                .get(idx)
                .is_some_and(|agent| matches!(agent.status, AgentStatus::Exited(_))),
            None => false,
        }
    }

    pub(crate) fn dismiss_selected_exited_session(&mut self) {
        // The session already exited (DB was finalized in handle_*_exit); just drop
        // it from the list and let the selection clamp to a neighbour.
        let Some(target) = self.selected_session_target() else {
            return;
        };
        if self.remove_session_target(target) {
            self.finish_session_mutation();
        }
    }

    /// Revive a selected orphaned session by re-launching its CLI with stored
    /// args in its stored workdir.
    pub(crate) fn revive_selected_orphaned_session(&mut self) {
        let Some(AgentEntry::Orphaned(idx)) = self.selected_agent() else {
            return;
        };
        let idx = *idx;
        let Some(session) = self.orphaned_sessions.get(idx).cloned() else {
            return;
        };

        let home = dirs::home_dir().unwrap_or_default();
        let canopy_config = crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy"));
        let (cols, rows) = Self::session_panel_size();
        let current_boot_id = crate::system::boot_id();

        // Remove from orphaned list first.
        self.orphaned_sessions.remove(idx);
        self.resume_interactive_session(
            &session,
            &canopy_config,
            cols,
            rows,
            current_boot_id.as_deref(),
        );
        self.finish_session_mutation();
    }

    /// Dismiss (remove) a selected orphaned session without reviving it.
    pub(crate) fn dismiss_selected_orphaned_session(&mut self) {
        let Some(AgentEntry::Orphaned(idx)) = self.selected_agent() else {
            return;
        };
        let idx = *idx;
        self.orphaned_sessions.remove(idx);
        self.finish_session_mutation();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_after_mutation_follows_anchor_shifted_up() {
        // "Down jump" scenario: B was reaped while cursor had moved to C (old
        // index 2); C is now at index 1 and must remain selected.
        assert_eq!(selection_after_mutation(&["A", "C", "D"], Some("C"), 2), 1);
    }

    #[test]
    fn selection_after_mutation_up_navigation_unaffected() {
        assert_eq!(selection_after_mutation(&["A", "C", "D"], Some("A"), 0), 0);
    }

    #[test]
    fn selection_after_mutation_clamps_when_anchor_missing() {
        assert_eq!(selection_after_mutation(&["A", "C", "D"], Some("B"), 2), 2);
    }

    #[test]
    fn selection_after_mutation_clamps_to_last_when_prev_out_of_range() {
        assert_eq!(selection_after_mutation(&["A", "C"], Some("Z"), 5), 1);
    }

    #[test]
    fn selection_after_mutation_empty_list_is_zero() {
        assert_eq!(selection_after_mutation(&[], Some("A"), 3), 0);
    }

    #[test]
    fn strip_ansi_codes_plain_text() {
        assert_eq!(strip_ansi_codes("hello"), "hello");
    }

    #[test]
    fn strip_ansi_codes_with_escapes() {
        assert_eq!(strip_ansi_codes("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn strip_ansi_codes_multiple_escapes() {
        assert_eq!(
            strip_ansi_codes("\x1b[1m\x1b[32mbold green\x1b[0m"),
            "bold green"
        );
    }

    #[test]
    fn strip_ansi_codes_empty() {
        assert_eq!(strip_ansi_codes(""), "");
    }

    #[test]
    fn strip_ansi_codes_no_escapes() {
        assert_eq!(strip_ansi_codes("no escapes here"), "no escapes here");
    }

    #[test]
    fn strip_ansi_codes_complex_sequence() {
        assert_eq!(strip_ansi_codes("\x1b[38;5;196mred256\x1b[0m"), "red256");
    }

    #[test]
    fn reverse_sorted_indices_basic() {
        assert_eq!(reverse_sorted_indices(vec![1, 3, 2]), vec![3, 2, 1]);
    }

    #[test]
    fn reverse_sorted_indices_empty() {
        assert_eq!(reverse_sorted_indices(vec![]), Vec::<usize>::new());
    }

    #[test]
    fn reverse_sorted_indices_single() {
        assert_eq!(reverse_sorted_indices(vec![5]), vec![5]);
    }

    #[test]
    fn reverse_sorted_indices_already_sorted() {
        assert_eq!(reverse_sorted_indices(vec![1, 2, 3]), vec![3, 2, 1]);
    }

    #[test]
    fn reverse_sorted_indices_duplicates() {
        assert_eq!(reverse_sorted_indices(vec![2, 2, 1, 3]), vec![3, 2, 2, 1]);
    }

    #[test]
    fn selection_after_mutation_no_anchor() {
        assert_eq!(selection_after_mutation(&["A", "B", "C"], None, 1), 1);
    }

    #[test]
    fn selection_after_mutation_no_anchor_clamps() {
        assert_eq!(selection_after_mutation(&["A", "B"], None, 5), 1);
    }

    #[test]
    fn selection_after_mutation_single_element() {
        assert_eq!(selection_after_mutation(&["A"], Some("A"), 0), 0);
    }

    #[test]
    fn selection_after_mutation_anchor_first() {
        assert_eq!(selection_after_mutation(&["X", "Y", "Z"], Some("X"), 2), 0);
    }

    #[test]
    fn selection_after_mutation_anchor_last() {
        assert_eq!(selection_after_mutation(&["X", "Y", "Z"], Some("Z"), 0), 2);
    }

    #[test]
    fn selection_after_mutation_no_anchor_empty() {
        assert_eq!(selection_after_mutation(&[], None, 0), 0);
    }

    #[test]
    fn brain_needs_reinit_none() {
        assert!(brain_needs_reinit(&None, 10, 10));
    }

    #[test]
    fn brain_needs_reinit_matching_dims() {
        let brain = make_brain(10, 10, 3);
        assert!(!brain_needs_reinit(&Some(brain), 10, 10));
    }

    #[test]
    fn brain_needs_reinit_different_dims() {
        let brain = make_brain(10, 10, 3);
        assert!(brain_needs_reinit(&Some(brain), 20, 20));
    }

    #[test]
    fn brain_needs_reinit_different_rows() {
        let brain = make_brain(10, 10, 3);
        assert!(brain_needs_reinit(&Some(brain), 20, 10));
    }

    #[test]
    fn brain_needs_reinit_different_cols() {
        let brain = make_brain(10, 10, 3);
        assert!(brain_needs_reinit(&Some(brain), 10, 20));
    }

    #[test]
    fn effective_brain_dims_small() {
        let (cols, rows) = effective_brain_dims((100, 200));
        assert!(rows > 0);
        assert!(cols > 0);
        assert!(rows <= 200);
        assert!(cols <= 100);
    }

    #[test]
    fn effective_brain_dims_zero_area() {
        let (cols, rows) = effective_brain_dims((0, 0));
        assert!(cols > 0);
        assert!(rows > 0);
    }

    #[test]
    fn effective_brain_dims_exactly_minimum() {
        let (cols, rows) = effective_brain_dims((6, 3));
        assert_eq!(cols, 6);
        assert_eq!(rows, 3);
    }

    #[test]
    fn effective_brain_dims_below_minimum() {
        let (cols, rows) = effective_brain_dims((5, 2));
        assert!(rows > 0);
        assert!(cols > 0);
    }

    #[test]
    fn effective_brain_dims_large() {
        let (cols, rows) = effective_brain_dims((500, 1000));
        assert_eq!(cols, 500);
        assert_eq!(rows, 1000);
    }

    #[test]
    fn selection_after_mutation_anchor_present_at_same_position() {
        assert_eq!(selection_after_mutation(&["A", "B", "C"], Some("A"), 0), 0);
    }

    #[test]
    fn selection_after_mutation_anchor_at_end() {
        assert_eq!(selection_after_mutation(&["A", "B", "C"], Some("C"), 0), 2);
    }

    #[test]
    fn selection_after_mutation_single_element_no_anchor() {
        assert_eq!(selection_after_mutation(&["X"], None, 0), 0);
    }

    #[test]
    fn strip_ansi_codes_nested_sequences() {
        assert_eq!(
            strip_ansi_codes("\x1b[1m\x1b[31mbold red\x1b[0m\x1b[0m"),
            "bold red"
        );
    }

    #[test]
    fn strip_ansi_codes_color256() {
        assert_eq!(strip_ansi_codes("\x1b[38;5;42mhello\x1b[0m"), "hello");
    }

    #[test]
    fn strip_ansi_codes_rgb_color() {
        assert_eq!(
            strip_ansi_codes("\x1b[38;2;255;128;0mcolored\x1b[0m"),
            "colored"
        );
    }

    #[test]
    fn brain_needs_reinit_none_vs_none() {
        assert!(brain_needs_reinit(&None, 5, 5));
    }

    #[test]
    fn brain_needs_reinit_exact_match_no_reinit() {
        let brain = make_brain(15, 25, 10);
        assert!(!brain_needs_reinit(&Some(brain), 15, 25));
    }

    #[test]
    fn effective_brain_dims_width_exactly_minimum() {
        let (cols, _rows) = effective_brain_dims((6, 100));
        assert_eq!(cols, 6);
    }

    #[test]
    fn effective_brain_dims_height_exactly_minimum() {
        let (_cols, rows) = effective_brain_dims((100, 3));
        assert_eq!(rows, 3);
    }
}

#[cfg(test)]
mod close_terminal_session_tests {
    use super::*;
    use crate::db::Database;
    use crate::tui::terminal_history::load_history;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile, TempDir};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    /// `cat` is a lightweight stand-in shell. These tests drive its vt100
    /// parser directly with known bytes (via `feed`) instead of depending on
    /// real PTY timing, so what `cat` actually does with stdin never matters.
    fn spawn_test_terminal(name: &str) -> InteractiveAgent {
        InteractiveAgent::spawn_terminal(
            "cat",
            "/tmp",
            80,
            24,
            Some(name),
            &[],
            ratatui::style::Color::White,
        )
        .expect("spawn terminal")
    }

    fn feed(agent: &InteractiveAgent, bytes: &[u8]) {
        agent.vt.lock().expect("lock vt").process(bytes);
    }

    /// Builds an `App` with a real temp data dir and pre-populates
    /// `terminal_histories` the way `launch_terminal`/`resume_terminal_session`
    /// do (load-then-cache), so `close_terminal_session_at` has somewhere to
    /// persist into.
    fn app_with_history(name: &str) -> (App, TempDir) {
        let db = test_db();
        let data_dir = tempdir().expect("tempdir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        let hist = load_history(data_dir.path(), name);
        app.terminal_histories.insert(name.to_string(), hist);
        (app, data_dir)
    }

    #[test]
    fn close_replaces_stored_scrollback_not_appends_across_repeated_closes() {
        let (mut app, data_dir) = app_with_history("caolinita");

        let agent = spawn_test_terminal("caolinita");
        feed(&agent, b"first run output\r\n");
        app.terminal_agents.push(agent);
        assert!(app.close_terminal_session_at(0));

        let after_first = load_history(data_dir.path(), "caolinita");
        assert!(after_first
            .scrollback
            .iter()
            .any(|l| l.contains("first run output")));

        // Reopen (mirrors a fresh session with the same name) and close again
        // with different content.
        let agent2 = spawn_test_terminal("caolinita");
        feed(&agent2, b"second run output\r\n");
        app.terminal_agents.push(agent2);
        assert!(app.close_terminal_session_at(0));

        let after_second = load_history(data_dir.path(), "caolinita");
        assert!(after_second
            .scrollback
            .iter()
            .any(|l| l.contains("second run output")));
        assert!(
            !after_second
                .scrollback
                .iter()
                .any(|l| l.contains("first run output")),
            "closing again must replace, not append, the stored snapshot: {:?}",
            after_second.scrollback
        );
    }

    #[test]
    fn close_excludes_full_screen_program_output() {
        let (mut app, data_dir) = app_with_history("cuarzo");

        let agent = spawn_test_terminal("cuarzo");
        // Enter the alternate screen (DECSET 1049) and paint a frame, the way
        // vim/htop/a nested canopy would.
        feed(&agent, b"\x1b[?1049h");
        feed(&agent, b"fake full-screen UI chrome\r\n");
        assert!(agent.in_alternate_screen());
        app.terminal_agents.push(agent);

        assert!(app.close_terminal_session_at(0));

        let after = load_history(data_dir.path(), "cuarzo");
        assert!(
            after.scrollback.is_empty(),
            "closing while a full-screen program is active must not persist its frame \
             as scrollback text: {:?}",
            after.scrollback
        );
    }

    #[test]
    fn close_captures_normally_once_full_screen_program_has_exited() {
        let (mut app, data_dir) = app_with_history("session");

        let agent = spawn_test_terminal("session");
        feed(&agent, b"\x1b[?1049h");
        feed(&agent, b"vim frame\r\n");
        feed(&agent, b"\x1b[?1049l"); // back to the primary screen / shell
        feed(&agent, b"shell prompt$\r\n");
        assert!(!agent.in_alternate_screen());
        app.terminal_agents.push(agent);

        assert!(app.close_terminal_session_at(0));

        let after = load_history(data_dir.path(), "session");
        assert!(after.scrollback.iter().any(|l| l.contains("shell prompt$")));
        assert!(
            !after.scrollback.iter().any(|l| l.contains("vim frame")),
            "content painted only in the alternate screen must never reach primary \
             scrollback: {:?}",
            after.scrollback
        );
    }
}
