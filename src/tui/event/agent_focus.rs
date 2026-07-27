use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use super::context_transfer::{active_split_session_name, resolve_session};
use super::home_preview::handle_playground_key;
use super::knowledge_dialog::{edit_knowledge_dialog, open_knowledge_dialog};
use super::search_picker::handle_suggestion_picker_key;
use super::terminal_warp::{
    handle_terminal_direct_pty_key, handle_terminal_warp_key, record_terminal_command,
};
use crate::tui::agent::{key_to_bytes, InteractiveAgent};
use crate::tui::app::types::{AgentEntry, App, Focus, ProjectTab, SidebarLayer};

#[derive(Clone, Copy)]
enum FocusedAgent {
    Interactive(usize),
    Terminal(usize),
}

pub fn handle_agent_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    if app.sidebar_layer == SidebarLayer::Knowledge && app.project_focus.is_some() {
        handle_project_focus_key(app, code, modifiers);
        return Ok(());
    }

    if app.suggestion_picker.is_some() {
        return handle_suggestion_picker_key(app, code);
    }
    if handle_playground_key(app, code, modifiers) {
        return Ok(());
    }

    if handle_split_picker_key(app, code)
        || handle_background_agent_key(app, code, modifiers)
        || handle_focus_shortcuts(app, code, modifiers)
    {
        return Ok(());
    }

    let Some(target) = resolve_focused_agent(app) else {
        return Ok(());
    };

    if handle_scroll_navigation(app, target, code, modifiers) {
        return Ok(());
    }

    reset_scroll_on_input(app, target, code);
    if handle_target_input(app, target, code, modifiers)? {
        return Ok(());
    }

    forward_key_to_focused_agent(app, target, code, modifiers);
    Ok(())
}

/// Keys while a project's Focus tab bar is open (`sidebar_layer ==
/// Knowledge`, `project_focus.is_some()`): arrows navigate the active tab's
/// list only — they never change tabs; Tab/Shift+Tab or `]`/`[` cycle tabs;
/// o/b/k/h jump directly; Esc returns to the sidebar (functional
/// requirement 4).
fn handle_project_focus_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    if app.knowledge_filter_mode {
        match code {
            KeyCode::Esc => {
                app.clear_knowledge_filter();
                app.exit_knowledge_filter_mode();
            }
            KeyCode::Enter => app.exit_knowledge_filter_mode(),
            KeyCode::Backspace => app.pop_knowledge_filter(),
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                app.append_knowledge_filter(c);
            }
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Esc | KeyCode::F(10) => {
            app.exit_project_focus();
            app.focus = Focus::Preview;
        }
        KeyCode::Tab | KeyCode::Char(']') => app.cycle_project_tab(true),
        KeyCode::BackTab | KeyCode::Char('[') => app.cycle_project_tab(false),
        KeyCode::Char(c) if ProjectTab::ALL.iter().any(|tab| tab.hotkey() == c) => {
            let tab = ProjectTab::ALL
                .into_iter()
                .find(|tab| tab.hotkey() == c)
                .unwrap();
            app.open_project_tab(tab);
        }
        KeyCode::Down => app.select_next(),
        KeyCode::Up => app.select_prev(),
        KeyCode::Char('/') if app.project_focus == Some(ProjectTab::Knowledge) => {
            app.enter_knowledge_filter_mode();
        }
        KeyCode::Char('e') if app.project_focus == Some(ProjectTab::Knowledge) => {
            edit_knowledge_dialog(app);
        }
        KeyCode::Char('n') if app.project_focus == Some(ProjectTab::Knowledge) => {
            open_knowledge_dialog(app);
        }
        KeyCode::F(4) if app.project_focus == Some(ProjectTab::Knowledge) => {
            let _ = app.delete_selected_knowledge();
        }
        _ => {}
    }
}

fn handle_split_picker_key(app: &mut App, code: KeyCode) -> bool {
    if !app.split_picker_open {
        return false;
    }

    match code {
        KeyCode::Down => cycle_split_picker(app, true),
        KeyCode::Up => cycle_split_picker(app, false),
        KeyCode::Tab => toggle_split_orientation(app),
        KeyCode::Enter => app.create_split(),
        KeyCode::Esc => app.split_picker_open = false,
        _ => {}
    }

    true
}

fn cycle_split_picker(app: &mut App, forward: bool) {
    let len = app.split_picker_sessions.len();
    if len == 0 {
        return;
    }

    app.split_picker_idx = if forward {
        (app.split_picker_idx + 1) % len
    } else {
        app.split_picker_idx.checked_sub(1).unwrap_or(len - 1)
    };
}

fn toggle_split_orientation(app: &mut App) {
    app.split_picker_orientation = match app.split_picker_orientation {
        crate::domain::models::SplitOrientation::Horizontal => {
            crate::domain::models::SplitOrientation::Vertical
        }
        crate::domain::models::SplitOrientation::Vertical => {
            crate::domain::models::SplitOrientation::Horizontal
        }
    };
}

fn handle_background_agent_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if matches!(
        app.selected_agent(),
        Some(AgentEntry::Interactive(_))
            | Some(AgentEntry::Terminal(_))
            | Some(AgentEntry::Group(_))
            | Some(AgentEntry::Orphaned(_))
    ) {
        return false;
    }

    // Let cross-section focus navigation fall through to the agent-cycle
    // shortcut; otherwise the cursor gets stuck on the background section
    // because `Shift+Up`/`Shift+Down` would be swallowed as log scrolling.
    if is_focus_cycle_key(code, modifiers) {
        return false;
    }

    match code {
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::F(10) => {
            app.active_split_id = None;
            app.focus = Focus::Preview;
        }
        KeyCode::Down | KeyCode::Char('j') => app.scroll_log_down(),
        KeyCode::Up | KeyCode::Char('k') => app.scroll_log_up(),
        KeyCode::Char('q') => app.running = false,
        KeyCode::F(1) => app.show_legend = !app.show_legend,
        KeyCode::Char('e') if !app.agents_rag_focused => app.open_edit_dialog(),
        _ => {}
    }

    true
}

/// Cross-section focus navigation (`Shift+Up`/`Shift+Down`), handled by
/// [`handle_agent_cycle_shortcut`]. Kept as a pure predicate so the background
/// key handler can defer these keys instead of consuming them as log scrolling.
fn is_focus_cycle_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::SHIFT) && matches!(code, KeyCode::Up | KeyCode::Down)
}

fn handle_focus_shortcuts(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    handle_context_transfer_shortcut(app, code, modifiers)
        || handle_split_picker_shortcut(app, code, modifiers)
        || handle_split_panel_focus_shortcut(app, code, modifiers)
        || handle_dismiss_exited_session(app, code)
        || handle_orphaned_session_key(app, code)
        || handle_preview_shortcut(app, code)
        || handle_termination_shortcut(app, code, modifiers)
        || handle_legend_shortcut(app, code)
        || handle_agent_cycle_shortcut(app, code, modifiers)
}

/// Whether an Esc/F10 press should dismiss a finished session instead of exiting
/// focus or reaching the PTY. Only applies to a single (non-split) selected
/// session that has already exited. Pure for testability.
fn dismisses_exited_session(code: KeyCode, in_split: bool, selected_exited: bool) -> bool {
    matches!(code, KeyCode::Esc | KeyCode::F(10)) && !in_split && selected_exited
}

fn handle_dismiss_exited_session(app: &mut App, code: KeyCode) -> bool {
    if !dismisses_exited_session(
        code,
        app.active_split_id.is_some(),
        app.selected_session_is_exited(),
    ) {
        return false;
    }
    app.dismiss_selected_exited_session();
    true
}

/// Handle keys on a selected orphaned session: 'r' to revive, 'd' to dismiss.
fn handle_orphaned_session_key(app: &mut App, code: KeyCode) -> bool {
    let is_orphaned = matches!(app.selected_agent(), Some(AgentEntry::Orphaned(_)));
    if !is_orphaned {
        return false;
    }
    match code {
        KeyCode::Char('r') => {
            app.revive_selected_orphaned_session();
            true
        }
        KeyCode::Char('d') | KeyCode::Esc | KeyCode::F(10) => {
            app.dismiss_selected_orphaned_session();
            true
        }
        _ => false,
    }
}

fn handle_context_transfer_shortcut(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if code != KeyCode::Char('t') || !modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }

    if app.active_split_id.is_some() {
        app.open_context_transfer_for_split();
        return true;
    }

    if matches!(
        app.selected_agent(),
        Some(AgentEntry::Interactive(_)) | Some(AgentEntry::Terminal(_))
    ) {
        app.open_context_transfer_modal();
    }

    true
}

fn handle_split_picker_shortcut(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if code != KeyCode::Char('s') || !modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }

    app.open_split_picker();
    true
}

fn handle_split_panel_focus_shortcut(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    if !modifiers.contains(KeyModifiers::SHIFT) {
        return false;
    }

    match code {
        KeyCode::Left => app.split_right_focused = false,
        KeyCode::Right => app.split_right_focused = true,
        _ => return false,
    }

    true
}

fn handle_preview_shortcut(app: &mut App, code: KeyCode) -> bool {
    if code != KeyCode::F(10) {
        return false;
    }

    app.active_split_id = None;
    app.focus = Focus::Preview;
    true
}

fn handle_termination_shortcut(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if code != KeyCode::F(4) {
        return false;
    }

    if modifiers.contains(KeyModifiers::SHIFT) {
        return terminate_split_session_if_present(app);
    }

    if app.active_split_id.is_some() {
        app.dissolve_split();
        return true;
    }

    app.terminate_focused_session();
    true
}

fn terminate_split_session_if_present(app: &mut App) -> bool {
    if app.active_split_id.is_none() {
        return true;
    }

    app.terminate_focused_session();
    true
}

fn handle_legend_shortcut(app: &mut App, code: KeyCode) -> bool {
    if code != KeyCode::F(1) {
        return false;
    }

    app.show_legend = !app.show_legend;
    true
}

fn handle_agent_cycle_shortcut(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if !modifiers.contains(KeyModifiers::SHIFT) {
        return false;
    }

    let forward = match code {
        KeyCode::Down => true,
        KeyCode::Up => false,
        _ => return false,
    };

    if app.rag_info.has_rag_activity() {
        if try_cycle_from_playground(app, forward) {
            return true;
        }
        if try_cycle_through_focusable(app, forward) {
            return true;
        }
    }

    if forward {
        app.next_interactive();
    } else {
        app.prev_interactive();
    }

    app.update_agent_section_focus_on_change(0);
    true
}

fn try_cycle_from_playground(app: &mut App, forward: bool) -> bool {
    if !app.playground_active {
        return false;
    }

    app.deactivate_playground();
    if forward {
        app.next_interactive();
    } else {
        app.prev_interactive();
    }
    app.update_agent_section_focus_on_change(0);
    true
}

fn try_cycle_through_focusable(app: &mut App, forward: bool) -> bool {
    let focusable: Vec<usize> = app
        .agents
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            matches!(
                entry,
                AgentEntry::Interactive(_)
                    | AgentEntry::Terminal(_)
                    | AgentEntry::Group(_)
                    | AgentEntry::Agent(_)
                    | AgentEntry::Orphaned(_)
            )
        })
        .map(|(idx, _)| idx)
        .collect();

    if focusable.is_empty() {
        app.activate_playground();
        app.focus = Focus::Agent;
        app.update_agent_section_focus_on_change(0);
        return true;
    }

    if try_cycle_to_rag_info(app, forward, &focusable) {
        return true;
    }

    if try_cycle_from_rag_info(app, forward, &focusable) {
        return true;
    }

    advance_focusable_selection(app, forward, &focusable);
    true
}

fn try_cycle_to_rag_info(app: &mut App, forward: bool, focusable: &[usize]) -> bool {
    let current_pos = focusable
        .iter()
        .position(|&idx| idx == app.selected)
        .unwrap_or(0);
    let at_edge = if forward {
        current_pos + 1 >= focusable.len()
    } else {
        current_pos == 0
    };

    if !at_edge {
        return false;
    }

    if !app.agents_rag_focused {
        app.agents_rag_focused = true;
        app.focus = Focus::Agent;
        app.update_agent_section_focus_on_change(0);
        return true;
    }

    app.agents_rag_focused = false;
    app.selected = if forward { 0 } else { focusable.len() - 1 };
    app.focus = Focus::Agent;
    app.update_agent_section_focus_on_change(0);
    true
}

fn try_cycle_from_rag_info(app: &mut App, forward: bool, focusable: &[usize]) -> bool {
    if !app.agents_rag_focused {
        return false;
    }

    app.agents_rag_focused = false;
    app.selected = if forward { 0 } else { focusable.len() - 1 };
    app.focus = Focus::Agent;
    app.update_agent_section_focus_on_change(0);
    true
}

fn advance_focusable_selection(app: &mut App, forward: bool, focusable: &[usize]) {
    let current_pos = focusable
        .iter()
        .position(|&idx| idx == app.selected)
        .unwrap_or(0);
    let next_pos = if forward {
        (current_pos + 1) % focusable.len()
    } else {
        current_pos.checked_sub(1).unwrap_or(focusable.len() - 1)
    };
    app.selected = focusable[next_pos];
    app.focus = Focus::Agent;
    app.update_agent_section_focus_on_change(0);
}

fn resolve_focused_agent(app: &mut App) -> Option<FocusedAgent> {
    if app.active_split_id.is_some() {
        return resolve_split_focused_agent(app);
    }

    resolve_selected_focused_agent(app)
}

fn resolve_split_focused_agent(app: &mut App) -> Option<FocusedAgent> {
    let Some(session_name) = active_split_session_name(app) else {
        app.focus = Focus::Preview;
        return None;
    };
    let session_name = session_name.to_string();

    let (agent_vec, idx) = resolve_session(app, &session_name);
    resolve_agent_target(app, agent_vec, idx, Focus::Preview)
}

fn resolve_selected_focused_agent(app: &mut App) -> Option<FocusedAgent> {
    let target = match app.selected_agent() {
        Some(AgentEntry::Interactive(idx)) => FocusedAgent::Interactive(*idx),
        Some(AgentEntry::Terminal(idx)) => FocusedAgent::Terminal(*idx),
        _ => {
            app.focus = Focus::Home;
            return None;
        }
    };

    resolve_agent_target_for_selection(app, target)
}

fn resolve_agent_target_for_selection(app: &mut App, target: FocusedAgent) -> Option<FocusedAgent> {
    if focused_agent(app, target).is_some() {
        return Some(target);
    }

    app.focus = Focus::Preview;
    None
}

fn resolve_agent_target(
    app: &mut App,
    agent_vec: &str,
    idx: usize,
    invalid_focus: Focus,
) -> Option<FocusedAgent> {
    let target = match agent_vec {
        "interactive" => FocusedAgent::Interactive(idx),
        "terminal" => FocusedAgent::Terminal(idx),
        _ => {
            app.focus = invalid_focus;
            return None;
        }
    };

    if focused_agent(app, target).is_some() {
        return Some(target);
    }

    app.focus = invalid_focus;
    None
}

fn focused_agent(app: &App, target: FocusedAgent) -> Option<&InteractiveAgent> {
    match target {
        FocusedAgent::Interactive(idx) => app.interactive_agents.get(idx),
        FocusedAgent::Terminal(idx) => app.terminal_agents.get(idx),
    }
}

fn focused_agent_mut(app: &mut App, target: FocusedAgent) -> Option<&mut InteractiveAgent> {
    match target {
        FocusedAgent::Interactive(idx) => app.interactive_agents.get_mut(idx),
        FocusedAgent::Terminal(idx) => app.terminal_agents.get_mut(idx),
    }
}

fn handle_scroll_navigation(
    app: &mut App,
    target: FocusedAgent,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    let pty_owns_navigation =
        focused_agent(app, target).is_some_and(InteractiveAgent::in_alternate_screen);
    if modifiers.contains(KeyModifiers::SHIFT) && !pty_owns_navigation {
        if let Some((scroll_up, step)) = shift_scroll_request(code) {
            return scroll_focused_agent(app, target, scroll_up, step);
        }
    }

    if pty_owns_navigation {
        return false;
    }

    let scrolled = focused_agent(app, target).is_some_and(|agent| agent.scroll_offset > 0);
    let Some((scroll_up, step)) = standard_scroll_request(code, scrolled) else {
        return false;
    };

    scroll_focused_agent(app, target, scroll_up, step)
}

fn shift_scroll_request(code: KeyCode) -> Option<(bool, usize)> {
    match code {
        KeyCode::Up => Some((true, 3)),
        KeyCode::Down => Some((false, 3)),
        _ => None,
    }
}

fn standard_scroll_request(code: KeyCode, scrolled: bool) -> Option<(bool, usize)> {
    match code {
        KeyCode::Up if scrolled => Some((true, 3)),
        KeyCode::Down if scrolled => Some((false, 3)),
        KeyCode::PageUp => Some((true, 15)),
        KeyCode::PageDown => Some((false, 15)),
        _ => None,
    }
}

fn scroll_focused_agent(app: &mut App, target: FocusedAgent, scroll_up: bool, step: usize) -> bool {
    let Some(max_scroll) = focused_agent(app, target).map(InteractiveAgent::max_scroll) else {
        return false;
    };
    let Some(agent) = focused_agent_mut(app, target) else {
        return false;
    };

    if scroll_up {
        agent.scroll_offset = (agent.scroll_offset + step).min(max_scroll);
    } else {
        agent.scroll_offset = agent.scroll_offset.saturating_sub(step);
    }

    true
}

fn reset_scroll_on_input(app: &mut App, target: FocusedAgent, code: KeyCode) {
    if !matches!(
        code,
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Tab
    ) {
        return;
    }

    let Some(agent) = focused_agent_mut(app, target) else {
        return;
    };
    if agent.scroll_offset == 0 {
        return;
    }

    agent.scroll_offset = 0;
}

fn handle_target_input(
    app: &mut App,
    target: FocusedAgent,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<bool> {
    match target {
        FocusedAgent::Interactive(idx) => {
            handle_interactive_input(app, idx, code, modifiers);
            Ok(false)
        }
        FocusedAgent::Terminal(idx) => handle_terminal_input(app, idx, code, modifiers),
    }
}

fn handle_interactive_input(app: &mut App, idx: usize, code: KeyCode, modifiers: KeyModifiers) {
    let target = FocusedAgent::Interactive(idx);
    if code == KeyCode::Enter {
        record_interactive_prompt(app, idx);
        clear_input_buffer(app, target);
        return;
    }

    track_plain_input(app, target, code, modifiers);
}

fn record_interactive_prompt(app: &mut App, idx: usize) {
    if app.interactive_agents[idx].is_sensitive_input_active() {
        return;
    }

    let Some(captured) = trimmed_input_buffer(app, FocusedAgent::Interactive(idx)) else {
        return;
    };
    if captured.is_empty() {
        return;
    }

    app.interactive_agents[idx].record_prompt(&captured);
}

fn handle_terminal_input(
    app: &mut App,
    idx: usize,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<bool> {
    if code == KeyCode::Char('w') && modifiers.contains(KeyModifiers::CONTROL) {
        toggle_terminal_warp_mode(app, idx);
        return Ok(true);
    }

    if app.terminal_agents[idx].warp_mode {
        return handle_terminal_warp_input(app, idx, code, modifiers);
    }

    track_terminal_input(app, idx, code, modifiers);
    Ok(false)
}

fn toggle_terminal_warp_mode(app: &mut App, idx: usize) {
    app.terminal_agents[idx].warp_mode = !app.terminal_agents[idx].warp_mode;
    app.terminal_agents[idx].warp_passthrough = false;
}

fn handle_terminal_warp_input(
    app: &mut App,
    idx: usize,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<bool> {
    if app.terminal_agents[idx].should_bypass_warp_input() {
        handle_terminal_direct_pty_key(app, idx, code, modifiers)?;
        return Ok(true);
    }

    handle_terminal_warp_key(app, idx, code, modifiers)?;
    Ok(true)
}

fn track_terminal_input(app: &mut App, idx: usize, code: KeyCode, modifiers: KeyModifiers) {
    let target = FocusedAgent::Terminal(idx);
    if code == KeyCode::Enter {
        let captured = trimmed_input_buffer(app, target).unwrap_or_default();
        record_terminal_command(app, idx, &captured);
        clear_input_buffer(app, target);
        return;
    }
    if code == KeyCode::Tab {
        return;
    }

    track_plain_input(app, target, code, modifiers);
}

fn track_plain_input(app: &mut App, target: FocusedAgent, code: KeyCode, modifiers: KeyModifiers) {
    let KeyCode::Char(ch) = code else {
        if code == KeyCode::Backspace {
            pop_input_buffer(app, target);
        }
        return;
    };
    if modifiers.contains(KeyModifiers::CONTROL) {
        return;
    }

    let _ = with_input_buffer_mut(app, target, |input| input.push(ch));
}

fn trimmed_input_buffer(app: &App, target: FocusedAgent) -> Option<String> {
    let agent = focused_agent(app, target)?;
    let Ok(input) = agent.input_buffer.lock() else {
        return None;
    };

    Some(input.trim().to_string())
}

fn with_input_buffer_mut<R>(
    app: &mut App,
    target: FocusedAgent,
    f: impl FnOnce(&mut String) -> R,
) -> Option<R> {
    let agent = focused_agent_mut(app, target)?;
    let Ok(mut input) = agent.input_buffer.lock() else {
        return None;
    };

    Some(f(&mut input))
}

fn clear_input_buffer(app: &mut App, target: FocusedAgent) {
    let _ = with_input_buffer_mut(app, target, String::clear);
}

fn pop_input_buffer(app: &mut App, target: FocusedAgent) {
    let _ = with_input_buffer_mut(app, target, |input| {
        input.pop();
    });
}

fn forward_key_to_focused_agent(
    app: &mut App,
    target: FocusedAgent,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    let bytes = key_to_bytes(code, modifiers);
    if bytes.is_empty() {
        return;
    }

    let Some(agent) = focused_agent_mut(app, target) else {
        return;
    };
    let _ = agent.write_to_pty(&bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::AgentRepository;
    use crate::db::Database;
    use crate::domain::models::{Agent, Cli, Trigger};
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn cron_agent(id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "original prompt".to_string(),
            trigger: Some(Trigger::Cron {
                schedule_expr: "0 9 * * *".to_string(),
            }),
            cli: Cli::new("claude"),
            model: Some("original-model".to_string()),
            working_dir: Some("/original/dir".to_string()),
            enabled: true,
            enable_at: None,
            created_at: Utc::now(),
            log_path: "/tmp/test-cron.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 3,
        }
    }

    fn app_with_background_agent() -> App {
        let db = test_db();
        let agent = cron_agent("cron-1");
        db.upsert_agent(&agent).expect("seed agent");

        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(agent)];
        app.selected = 0;
        app.focus = Focus::Agent;
        app
    }

    #[test]
    fn e_key_opens_edit_dialog_for_focused_background_agent() {
        let mut app = app_with_background_agent();

        let handled = handle_background_agent_key(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);

        assert!(handled);
        assert!(matches!(app.focus, Focus::NewAgentDialog));
        assert!(app.new_agent_dialog.is_some());
    }

    #[test]
    fn e_key_does_not_open_edit_dialog_when_rag_info_is_focused() {
        let mut app = app_with_background_agent();
        app.agents_rag_focused = true;

        let handled = handle_background_agent_key(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);

        assert!(handled);
        assert!(matches!(app.focus, Focus::Agent));
        assert!(app.new_agent_dialog.is_none());
    }

    #[test]
    fn esc_exits_background_agent_focus_to_preview() {
        // Regression guard for T26: ESC must leave the background agent focus
        // and return to the preview pane, matching the panel's "Esc → back" hint.
        let mut app = app_with_background_agent();
        app.active_split_id = Some("split-1".to_string());

        let handled = handle_background_agent_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        assert!(handled);
        assert!(app.active_split_id.is_none());
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn f10_exits_background_agent_focus_to_preview() {
        // Regression guard for T26: F10 is an alternate exit key for the
        // background agent focus and must behave the same as ESC.
        let mut app = app_with_background_agent();
        app.active_split_id = Some("split-1".to_string());

        let handled = handle_background_agent_key(&mut app, KeyCode::F(10), KeyModifiers::NONE);

        assert!(handled);
        assert!(app.active_split_id.is_none());
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn h_exits_background_agent_focus_to_preview() {
        // Regression guard for T26: 'h' is an alternate exit key for the
        // background agent focus and must behave the same as ESC.
        let mut app = app_with_background_agent();
        app.active_split_id = Some("split-1".to_string());

        let handled = handle_background_agent_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);

        assert!(handled);
        assert!(app.active_split_id.is_none());
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn shift_arrows_are_focus_cycle_keys() {
        // These must reach the agent-cycle shortcut so navigation can leave the
        // background section in both directions instead of getting stuck.
        assert!(is_focus_cycle_key(KeyCode::Down, KeyModifiers::SHIFT));
        assert!(is_focus_cycle_key(KeyCode::Up, KeyModifiers::SHIFT));
    }

    #[test]
    fn plain_arrows_are_not_focus_cycle_keys() {
        // Without SHIFT the background handler keeps scrolling the agent log.
        assert!(!is_focus_cycle_key(KeyCode::Down, KeyModifiers::NONE));
        assert!(!is_focus_cycle_key(KeyCode::Up, KeyModifiers::NONE));
    }

    #[test]
    fn shift_non_arrows_are_not_focus_cycle_keys() {
        assert!(!is_focus_cycle_key(KeyCode::Char('j'), KeyModifiers::SHIFT));
        assert!(!is_focus_cycle_key(KeyCode::Left, KeyModifiers::SHIFT));
        assert!(!is_focus_cycle_key(KeyCode::PageDown, KeyModifiers::SHIFT));
    }

    #[test]
    fn esc_dismisses_exited_session() {
        assert!(dismisses_exited_session(KeyCode::Esc, false, true));
    }

    #[test]
    fn f10_dismisses_exited_session() {
        assert!(dismisses_exited_session(KeyCode::F(10), false, true));
    }

    #[test]
    fn split_active_does_not_dismiss_exited_session() {
        assert!(!dismisses_exited_session(KeyCode::Esc, true, true));
    }

    #[test]
    fn running_session_is_not_dismissed() {
        assert!(!dismisses_exited_session(KeyCode::Esc, false, false));
    }

    #[test]
    fn other_key_does_not_dismiss_exited_session() {
        assert!(!dismisses_exited_session(KeyCode::Char('x'), false, true));
    }

    #[test]
    fn shift_scroll_request_up() {
        assert_eq!(shift_scroll_request(KeyCode::Up), Some((true, 3)));
    }

    #[test]
    fn shift_scroll_request_down() {
        assert_eq!(shift_scroll_request(KeyCode::Down), Some((false, 3)));
    }

    #[test]
    fn shift_scroll_request_left() {
        assert_eq!(shift_scroll_request(KeyCode::Left), None);
    }

    #[test]
    fn shift_scroll_request_enter() {
        assert_eq!(shift_scroll_request(KeyCode::Enter), None);
    }

    #[test]
    fn standard_scroll_request_up_scrolled() {
        assert_eq!(standard_scroll_request(KeyCode::Up, true), Some((true, 3)));
    }

    #[test]
    fn standard_scroll_request_up_not_scrolled() {
        assert_eq!(standard_scroll_request(KeyCode::Up, false), None);
    }

    #[test]
    fn standard_scroll_request_down_scrolled() {
        assert_eq!(
            standard_scroll_request(KeyCode::Down, true),
            Some((false, 3))
        );
    }

    #[test]
    fn standard_scroll_request_down_not_scrolled() {
        assert_eq!(standard_scroll_request(KeyCode::Down, false), None);
    }

    #[test]
    fn standard_scroll_request_page_up() {
        assert_eq!(
            standard_scroll_request(KeyCode::PageUp, false),
            Some((true, 15))
        );
    }

    #[test]
    fn standard_scroll_request_page_down() {
        assert_eq!(
            standard_scroll_request(KeyCode::PageDown, false),
            Some((false, 15))
        );
    }

    #[test]
    fn standard_scroll_request_enter() {
        assert_eq!(standard_scroll_request(KeyCode::Enter, false), None);
    }

    #[test]
    fn standard_scroll_request_char() {
        assert_eq!(standard_scroll_request(KeyCode::Char('a'), false), None);
    }

    #[test]
    fn is_focus_cycle_key_ctrl_shift() {
        // contains(SHIFT) is true even when CONTROL is also set
        assert!(is_focus_cycle_key(
            KeyCode::Up,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn dismisses_exited_session_ctrl_esc() {
        assert!(!dismisses_exited_session(KeyCode::Esc, false, false));
    }

    #[test]
    fn dismisses_exited_session_f10_in_split() {
        assert!(!dismisses_exited_session(KeyCode::F(10), true, false));
    }

    #[test]
    fn dismisses_exited_session_f10_not_exited() {
        assert!(!dismisses_exited_session(KeyCode::F(10), false, false));
    }

    #[test]
    fn dismisses_exited_session_f10_split_and_exited() {
        assert!(!dismisses_exited_session(KeyCode::F(10), true, true));
    }

    #[test]
    fn dismisses_exited_session_various_keys() {
        assert!(!dismisses_exited_session(KeyCode::Char('a'), false, true));
        assert!(!dismisses_exited_session(KeyCode::Enter, false, true));
        assert!(!dismisses_exited_session(KeyCode::Tab, false, true));
        assert!(!dismisses_exited_session(KeyCode::Down, false, true));
    }

    #[test]
    fn shift_scroll_request_left_is_none() {
        assert!(shift_scroll_request(KeyCode::Left).is_none());
    }

    #[test]
    fn shift_scroll_request_right_is_none() {
        assert!(shift_scroll_request(KeyCode::Right).is_none());
    }

    #[test]
    fn shift_scroll_request_pageup_is_none() {
        assert!(shift_scroll_request(KeyCode::PageUp).is_none());
    }

    #[test]
    fn shift_scroll_request_pagedown_is_none() {
        assert!(shift_scroll_request(KeyCode::PageDown).is_none());
    }

    #[test]
    fn standard_scroll_request_pageup_scrolled() {
        assert_eq!(
            standard_scroll_request(KeyCode::PageUp, true),
            Some((true, 15))
        );
    }

    #[test]
    fn standard_scroll_request_pagedown_scrolled() {
        assert_eq!(
            standard_scroll_request(KeyCode::PageDown, true),
            Some((false, 15))
        );
    }

    #[test]
    fn standard_scroll_request_tab_is_none() {
        assert!(standard_scroll_request(KeyCode::Tab, false).is_none());
    }

    #[test]
    fn standard_scroll_request_esc_is_none() {
        assert!(standard_scroll_request(KeyCode::Esc, false).is_none());
    }

    #[test]
    fn is_focus_cycle_key_all_variants() {
        // Only Up/Down with SHIFT are focus cycle keys
        assert!(is_focus_cycle_key(KeyCode::Up, KeyModifiers::SHIFT));
        assert!(is_focus_cycle_key(KeyCode::Down, KeyModifiers::SHIFT));
        // Other keys with SHIFT are not
        assert!(!is_focus_cycle_key(KeyCode::Left, KeyModifiers::SHIFT));
        assert!(!is_focus_cycle_key(KeyCode::Right, KeyModifiers::SHIFT));
        // Without SHIFT, not cycle keys
        assert!(!is_focus_cycle_key(KeyCode::Up, KeyModifiers::NONE));
        assert!(!is_focus_cycle_key(KeyCode::Down, KeyModifiers::NONE));
        // Control alone is not enough
        assert!(!is_focus_cycle_key(
            KeyCode::Up,
            KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn dismisses_exited_session_all_combos() {
        // esc, not split, exited
        assert!(dismisses_exited_session(KeyCode::Esc, false, true));
        // esc, not split, not exited
        assert!(!dismisses_exited_session(KeyCode::Esc, false, false));
        // esc, split, exited
        assert!(!dismisses_exited_session(KeyCode::Esc, true, true));
        // esc, split, not exited
        assert!(!dismisses_exited_session(KeyCode::Esc, true, false));
        // f10, not split, exited
        assert!(dismisses_exited_session(KeyCode::F(10), false, true));
        // f10, not split, not exited
        assert!(!dismisses_exited_session(KeyCode::F(10), false, false));
        // f10, split, exited
        assert!(!dismisses_exited_session(KeyCode::F(10), true, true));
        // f10, split, not exited
        assert!(!dismisses_exited_session(KeyCode::F(10), true, false));
    }
}
