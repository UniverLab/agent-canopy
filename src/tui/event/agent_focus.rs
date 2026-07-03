use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use super::context_transfer::resolve_session;
use super::home_preview::handle_playground_key;
use super::search_picker::handle_suggestion_picker_key;
use super::terminal_warp::{
    handle_terminal_direct_pty_key, handle_terminal_warp_key, record_terminal_command,
};
use crate::tui::agent::{key_to_bytes, InteractiveAgent};
use crate::tui::app::types::{AgentEntry, App, Focus};

#[derive(Clone, Copy)]
enum FocusedAgent {
    Interactive(usize),
    Terminal(usize),
}

pub fn handle_agent_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
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
        || handle_preview_shortcut(app, code)
        || handle_termination_shortcut(app, code, modifiers)
        || handle_legend_shortcut(app, code)
        || handle_agent_cycle_shortcut(app, code, modifiers)
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

    app.sidebar_mode = crate::tui::app::SidebarMode::Agents;
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
    app.sidebar_mode = crate::tui::app::SidebarMode::Agents;
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
            )
        })
        .map(|(idx, _)| idx)
        .collect();

    if focusable.is_empty() {
        app.activate_playground();
        app.focus = Focus::Agent;
        app.sidebar_mode = crate::tui::app::SidebarMode::Agents;
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
        app.sidebar_mode = crate::tui::app::SidebarMode::Agents;
        return true;
    }

    app.agents_rag_focused = false;
    app.selected = if forward { 0 } else { focusable.len() - 1 };
    app.focus = Focus::Agent;
    app.sidebar_mode = crate::tui::app::SidebarMode::Agents;
    true
}

fn try_cycle_from_rag_info(app: &mut App, forward: bool, focusable: &[usize]) -> bool {
    if !app.agents_rag_focused {
        return false;
    }

    app.agents_rag_focused = false;
    app.selected = if forward { 0 } else { focusable.len() - 1 };
    app.focus = Focus::Agent;
    app.sidebar_mode = crate::tui::app::SidebarMode::Agents;
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
    app.sidebar_mode = crate::tui::app::SidebarMode::Agents;
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

    let (agent_vec, idx) = resolve_session(app, session_name);
    resolve_agent_target(app, agent_vec, idx, Focus::Preview)
}

fn active_split_session_name(app: &App) -> Option<&str> {
    let split_id = app.active_split_id.as_deref()?;
    let group = app.split_groups.iter().find(|group| group.id == split_id)?;

    Some(if app.split_right_focused {
        group.session_b.as_str()
    } else {
        group.session_a.as_str()
    })
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
}
