use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use super::context_transfer::{
    active_split_session_name, resolve_session, resolve_split_focused_terminal_like,
};
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
/// Knowledge`, `project_focus.is_some()`): plain ↑↓ navigate the active
/// tab's list; Tab/Shift+Tab, `]`/`[`, and Shift+←/→ all cycle tabs (the
/// same ring-stepping convention as the sidebar's own tab strip — see
/// `event::sidebar_tab_step_applies`); o/b/k/h jump directly; Esc returns to
/// the sidebar (functional requirement 4).
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
        KeyCode::Right if modifiers.contains(KeyModifiers::SHIFT) => {
            app.cycle_project_tab(true);
        }
        KeyCode::Left if modifiers.contains(KeyModifiers::SHIFT) => {
            app.cycle_project_tab(false);
        }
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

    app.split_picker_idx = crate::tui::selection::move_index(app.split_picker_idx, len, forward);
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

/// Keys canopy keeps for itself even while a focused child has claimed the
/// keyboard (see [`focused_child_claimed_keyboard`]). The single source of
/// truth for the reserved set (spec C23) — nothing below `handle_focus_shortcuts`
/// gets to opt out of it on its own.
///
/// The rule for earning a place here is **frame navigation**: a key that moves
/// between canopy's own panes rather than acting on the session's content. A
/// multiplexer that hands the inner program its application shortcuts still has
/// to keep the keys that get you out of, and between, its frames — otherwise
/// entering a session is a one-way door. Everything else yields.
///
/// Each entry is a key plus the modifiers that must be present; a match is
/// `modifiers.contains(required)`, not equality, so a terminal that reports
/// extra modifiers alongside them still resolves.
const RESERVED_FOCUS_KEYS: &[(KeyCode, KeyModifiers)] = &[
    // Leaves focus back to the sidebar. With focus left, every other canopy
    // shortcut is reachable again from there, so it's the one key that must
    // survive a child claiming everything else.
    (KeyCode::F(10), KeyModifiers::NONE),
    // Cross-section focus navigation (`handle_agent_cycle_shortcut`): moves the
    // selection between agents and sections without leaving focus. Reserved
    // after C23 shipped without it and made stepping between sessions
    // impossible from inside one.
    (KeyCode::Up, KeyModifiers::SHIFT),
    (KeyCode::Down, KeyModifiers::SHIFT),
    // Split-pane focus and the sidebar tab strip (`handle_split_panel_focus_shortcut`
    // and the global handler behind it) — the horizontal half of the same
    // frame navigation.
    (KeyCode::Left, KeyModifiers::SHIFT),
    (KeyCode::Right, KeyModifiers::SHIFT),
    // Context transfer (`handle_context_transfer_shortcut`). Deliberately not
    // frame navigation — a one-off exception the owner made with eyes open:
    // Codex also binds Ctrl+T, and losing that collision was judged worth it
    // to make the transfer reachable again from inside a claimed session. The
    // owner doesn't use Codex's binding and will rebind it on Codex's side.
    (KeyCode::Char('t'), KeyModifiers::CONTROL),
];

fn is_reserved_focus_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    RESERVED_FOCUS_KEYS
        .iter()
        .any(|(reserved, required)| *reserved == code && modifiers.contains(*required))
}

/// The agent a focus shortcut would currently apply to: the focused split
/// pane's session when a split is active, otherwise the sidebar selection.
/// A read-only mirror of [`resolve_focused_agent`] — that one mutates
/// `app.focus` on a miss, which a predicate must not do.
fn currently_focused_target(app: &App) -> Option<FocusedAgent> {
    if app.active_split_id.is_some() {
        let (is_terminal, idx) = resolve_split_focused_terminal_like(app)?;
        return Some(if is_terminal {
            FocusedAgent::Terminal(idx)
        } else {
            FocusedAgent::Interactive(idx)
        });
    }

    match app.selected_agent() {
        Some(AgentEntry::Interactive(idx)) => Some(FocusedAgent::Interactive(*idx)),
        Some(AgentEntry::Terminal(idx)) => Some(FocusedAgent::Terminal(*idx)),
        _ => None,
    }
}

/// True once the child running in the currently focused session has
/// signaled it wants raw control of the keyboard: it entered the alternate
/// screen, or it pushed Kitty keyboard protocol flags (`CSI > flags u`).
/// Codex (spec C23) negotiates Kitty without ever using the alternate
/// screen, so either signal alone must be sufficient — neither is dropped.
pub(crate) fn focused_child_claimed_keyboard(app: &App) -> bool {
    // Ownership boundary (CT6): only the child in actual `Focus::Agent`
    // owns the keyboard. A session merely selected/previewed in
    // `Focus::Preview` never claims arrows — preview navigation owns them.
    // This is why preview arrows were sometimes swallowed: without the
    // focus gate, a previewed child in the alternate screen (or with Kitty
    // keyboard flags negotiated) counted as "focused" and its claim reached
    // preview-level dispatch.
    if app.focus != Focus::Agent {
        return false;
    }
    let Some(target) = currently_focused_target(app) else {
        return false;
    };
    focused_agent(app, target)
        .is_some_and(|agent| agent.in_alternate_screen() || agent.kitty_keyboard_negotiated())
}

fn handle_focus_shortcuts(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if !is_reserved_focus_key(code, modifiers) && focused_child_claimed_keyboard(app) {
        return false;
    }

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

/// Shift+←/→ moves focus between the two split panes. Only meaningful (and
/// only reachable — see `event::sidebar_tab_step_applies`) while a split is
/// active: without one, the global handler claims Shift+←/→ first to step
/// the sidebar tab strip instead, so this never sees the key in that case.
/// The `active_split_id` check here is a second, defensive guarantee of the
/// same contract, not load-bearing given today's call order.
fn handle_split_panel_focus_shortcut(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    if !modifiers.contains(KeyModifiers::SHIFT) || app.active_split_id.is_none() {
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
    let next_pos = crate::tui::selection::move_index(current_pos, focusable.len(), forward);
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
            effort: None,
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

    fn app_with_project_focus(tab: ProjectTab) -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.projects = vec![crate::domain::project::Project::new("/tmp/proj")];
        app.sidebar_layer = SidebarLayer::Knowledge;
        app.selected_project = 0;
        app.project_focus = Some(tab);
        app.focus = Focus::Agent;
        app
    }

    #[test]
    fn shift_right_steps_project_tab_forward() {
        let mut app = app_with_project_focus(ProjectTab::Overview);

        handle_project_focus_key(&mut app, KeyCode::Right, KeyModifiers::SHIFT);

        assert_eq!(app.project_focus, Some(ProjectTab::Backlog));
    }

    #[test]
    fn shift_left_steps_project_tab_backward_and_wraps() {
        let mut app = app_with_project_focus(ProjectTab::Overview);

        handle_project_focus_key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);

        assert_eq!(
            app.project_focus,
            Some(ProjectTab::History),
            "stepping left off the first tab wraps to the last"
        );
    }

    #[test]
    fn plain_arrows_do_not_step_project_tabs() {
        // Plain ↑↓ navigate the active tab's list; plain ←/→ are unclaimed
        // here (Shift+←/→ is the tab-stepping binding).
        let mut app = app_with_project_focus(ProjectTab::Overview);

        handle_project_focus_key(&mut app, KeyCode::Right, KeyModifiers::NONE);

        assert_eq!(app.project_focus, Some(ProjectTab::Overview));
    }

    #[test]
    fn split_panel_focus_shortcut_is_a_noop_without_an_active_split() {
        // Without a split, Shift+←/→ must not be consumed here — the global
        // handler claims it first to step the sidebar tab strip instead.
        let mut app = app_with_background_agent();
        app.active_split_id = None;

        let handled =
            handle_split_panel_focus_shortcut(&mut app, KeyCode::Right, KeyModifiers::SHIFT);

        assert!(!handled);
        assert!(!app.split_right_focused);
    }

    #[test]
    fn split_panel_focus_shortcut_still_works_with_an_active_split() {
        let mut app = app_with_background_agent();
        app.active_split_id = Some("split-1".to_string());

        let handled =
            handle_split_panel_focus_shortcut(&mut app, KeyCode::Right, KeyModifiers::SHIFT);

        assert!(handled);
        assert!(app.split_right_focused);
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
        assert!(!is_focus_cycle_key(KeyCode::Up, KeyModifiers::CONTROL));
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

// C23: `handle_focus_shortcuts` must yield every key but F10 once a focused
// child has claimed the keyboard (alternate screen or Kitty keyboard
// protocol push) — Codex negotiates Kitty without ever entering the
// alternate screen, which is the reported bug. Spawns a real (harmless)
// `cat` child and drives its `vt` / `kitty_keyboard_flags` state directly,
// mirroring the pattern established for C6/C20.
#[cfg(test)]
mod focus_shortcuts_keyboard_claim_tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::models::{Cli, SplitGroup, SplitOrientation};
    use chrono::Utc;
    use ratatui::style::Color;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn spawn_cat_agent(name: &str) -> InteractiveAgent {
        InteractiveAgent::spawn(
            Cli::new("cat"),
            ".",
            80,
            24,
            None,
            None,
            Color::Reset,
            Some(name),
            &[],
            None,
            None,
            None,
        )
        .expect("spawn cat as a stand-in interactive child")
    }

    fn app_with_interactive_agent(agent: InteractiveAgent) -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.interactive_agents = vec![agent];
        app.agents = vec![AgentEntry::Interactive(0)];
        app.selected = 0;
        app.focus = Focus::Agent;
        app
    }

    #[test]
    fn reserved_focus_keys_are_leave_focus_and_frame_navigation() {
        // The list is frame navigation, plus one deliberate exception: leave
        // focus, step between canopy's own panes, and Ctrl+T for context
        // transfer. Everything else that acts on the session's content is
        // the child's.
        assert_eq!(
            RESERVED_FOCUS_KEYS,
            [
                (KeyCode::F(10), KeyModifiers::NONE),
                (KeyCode::Up, KeyModifiers::SHIFT),
                (KeyCode::Down, KeyModifiers::SHIFT),
                (KeyCode::Left, KeyModifiers::SHIFT),
                (KeyCode::Right, KeyModifiers::SHIFT),
                (KeyCode::Char('t'), KeyModifiers::CONTROL),
            ]
        );
        assert!(is_reserved_focus_key(KeyCode::F(10), KeyModifiers::NONE));
        // Ownership of this key was reversed on purpose: the owner decided
        // context transfer is worth more than Codex's own Ctrl+T and will
        // rebind it on Codex's side.
        assert!(is_reserved_focus_key(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn shift_arrows_stay_with_canopy_while_a_child_claims_the_keyboard() {
        // Regression: C23 shipped with F10 as the only reserved key, which
        // made Shift+arrows dead from inside a focused session — there was no
        // way to step to another agent without leaving focus first.
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right] {
            assert!(
                is_reserved_focus_key(code, KeyModifiers::SHIFT),
                "Shift+{code:?} is frame navigation and must stay with canopy"
            );
            assert!(
                !is_reserved_focus_key(code, KeyModifiers::NONE),
                "a bare arrow is the child's"
            );
        }
    }

    #[test]
    fn kitty_negotiated_child_has_ctrl_t_reserved_for_context_transfer() {
        // Ownership of Ctrl+T was reversed on purpose (see
        // `RESERVED_FOCUS_KEYS`): Codex's real shape is Kitty-negotiated
        // without ever entering the alternate screen, and without this
        // reservation the transfer had no working entry point at all.
        let agent = spawn_cat_agent("codex-like");
        *agent.kitty_keyboard_flags.lock().expect("lock") = Some(7);
        assert!(agent.kitty_keyboard_negotiated());
        assert!(!agent.in_alternate_screen());
        let mut app = app_with_interactive_agent(agent);

        let handled = handle_focus_shortcuts(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        assert!(
            handled,
            "Ctrl+T must open context transfer even with a claimed keyboard"
        );
        assert!(matches!(app.focus, Focus::ContextTransfer));
        app.interactive_agents[0].kill();
    }

    #[test]
    fn kitty_negotiated_child_does_not_have_a_second_shortcut_consumed() {
        // Proves the fix is the ownership contract, not a Ctrl+T-only patch:
        // Ctrl+S (split picker) must yield too.
        let agent = spawn_cat_agent("codex-like-2");
        *agent.kitty_keyboard_flags.lock().expect("lock") = Some(7);
        let mut app = app_with_interactive_agent(agent);

        let handled = handle_focus_shortcuts(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

        assert!(!handled, "Ctrl+S must reach the Kitty-negotiated child");
        assert!(!app.split_picker_open);
        app.interactive_agents[0].kill();
    }

    #[test]
    fn kitty_negotiated_child_still_yields_f10_and_leaves_focus() {
        let agent = spawn_cat_agent("codex-like-3");
        *agent.kitty_keyboard_flags.lock().expect("lock") = Some(7);
        let mut app = app_with_interactive_agent(agent);

        let handled = handle_focus_shortcuts(&mut app, KeyCode::F(10), KeyModifiers::NONE);

        assert!(handled, "F10 is the one reserved key");
        assert!(matches!(app.focus, Focus::Preview));
        app.interactive_agents[0].kill();
    }

    #[test]
    fn alternate_screen_child_without_kitty_behaves_identically() {
        let agent = spawn_cat_agent("vim-like");
        agent.vt.lock().expect("vt lock").process(b"\x1b[?1049h");
        assert!(agent.in_alternate_screen());
        assert!(!agent.kitty_keyboard_negotiated());
        let mut app = app_with_interactive_agent(agent);

        assert!(
            handle_focus_shortcuts(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL),
            "Ctrl+T is reserved for context transfer regardless of which \
             signal claimed the keyboard"
        );
        // Opening the modal moved focus to ContextTransfer; re-establish
        // Agent focus so the F4/F10 probes below still exercise the claimed
        // focused child (in production the dispatcher re-routes by the new
        // focus after a handled key, so a single call never evaluates later
        // shortcuts under a mutated focus).
        app.focus = Focus::Agent;
        assert!(!handle_focus_shortcuts(
            &mut app,
            KeyCode::F(4),
            KeyModifiers::NONE
        ));
        assert!(handle_focus_shortcuts(
            &mut app,
            KeyCode::F(10),
            KeyModifiers::NONE
        ));
        assert!(matches!(app.focus, Focus::Preview));

        app.interactive_agents[0].kill();
    }

    #[test]
    fn neither_signal_behaves_exactly_like_today() {
        let agent = spawn_cat_agent("plain-shell");
        assert!(!agent.in_alternate_screen());
        assert!(!agent.kitty_keyboard_negotiated());
        let mut app = app_with_interactive_agent(agent);

        let handled = handle_focus_shortcuts(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        assert!(
            handled,
            "an unclaimed child must not change today's Ctrl+T behavior"
        );
        app.interactive_agents[0].kill();
    }

    #[test]
    fn no_focused_agent_is_unaffected() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.focus = Focus::Agent;

        assert!(!focused_child_claimed_keyboard(&app));
        let handled = handle_focus_shortcuts(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        // Unaffected by this spec: `handle_context_transfer_shortcut` still
        // consumes Ctrl+T unconditionally when it matches, agent or not.
        assert!(handled);
    }

    #[test]
    fn split_predicate_reads_the_focused_pane_not_the_other() {
        let left = spawn_cat_agent("left-term");
        let right = spawn_cat_agent("right-term");
        *right.kitty_keyboard_flags.lock().expect("lock") = Some(7);

        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.interactive_agents = vec![left, right];
        app.split_groups.push(SplitGroup {
            id: "split-1".to_string(),
            orientation: SplitOrientation::Horizontal,
            session_a: "left-term".to_string(),
            session_b: "right-term".to_string(),
            created_at: Utc::now(),
        });
        app.active_split_id = Some("split-1".to_string());
        app.focus = Focus::Agent;

        // Right panel focused: the claimed child owns the keyboard. Ctrl+S
        // (split picker) is unreserved content, unlike Ctrl+T, so it still
        // probes the claim.
        app.split_right_focused = true;
        assert!(!handle_focus_shortcuts(
            &mut app,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL
        ));

        // Left panel focused: its unclaimed child leaves Ctrl+S with canopy —
        // the other pane's claim must not leak across.
        app.split_right_focused = false;
        assert!(handle_focus_shortcuts(
            &mut app,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL
        ));

        app.interactive_agents[0].kill();
        app.interactive_agents[1].kill();
    }

    // CT6 ownership matrix: only a *focused* child may claim the keyboard.
    // A session merely selected in Preview never does — otherwise its
    // alternate-screen/Kitty state swallows preview-level Up/Down and the
    // arrows stop navigating between sessions.

    #[test]
    fn previewed_kitty_claimed_child_does_not_claim_keyboard() {
        let agent = spawn_cat_agent("previewed-codex-like");
        *agent.kitty_keyboard_flags.lock().expect("lock") = Some(7);
        assert!(agent.kitty_keyboard_negotiated());
        let mut app = app_with_interactive_agent(agent);
        app.focus = Focus::Preview;

        assert!(
            !focused_child_claimed_keyboard(&app),
            "a Kitty-negotiated child that is only previewed must not claim arrows"
        );
        app.interactive_agents[0].kill();
    }

    #[test]
    fn previewed_alternate_screen_child_does_not_claim_keyboard() {
        let agent = spawn_cat_agent("previewed-vim-like");
        agent.vt.lock().expect("vt lock").process(b"\x1b[?1049h");
        assert!(agent.in_alternate_screen());
        let mut app = app_with_interactive_agent(agent);
        app.focus = Focus::Preview;

        assert!(
            !focused_child_claimed_keyboard(&app),
            "an alternate-screen child that is only previewed must not claim arrows"
        );
        app.interactive_agents[0].kill();
    }

    #[test]
    fn focused_claimed_child_still_claims_keyboard() {
        // The focus gate narrows the claim; it must not remove it. A claimed
        // child in actual Agent focus still owns ordinary keys.
        let agent = spawn_cat_agent("focused-codex-like");
        *agent.kitty_keyboard_flags.lock().expect("lock") = Some(7);
        let mut app = app_with_interactive_agent(agent);
        assert!(matches!(app.focus, Focus::Agent));

        assert!(focused_child_claimed_keyboard(&app));
        app.interactive_agents[0].kill();
    }

    #[test]
    fn previewed_claimed_child_does_not_block_unreserved_shortcuts() {
        // Same ownership rule through `handle_focus_shortcuts`: with Preview
        // focus, an unreserved key (Ctrl+S) reaches canopy even though the
        // selected child negotiated Kitty — the claim cannot reach
        // preview-level dispatch. Two sessions so the split picker can open.
        let agent = spawn_cat_agent("previewed-ctrl-s");
        *agent.kitty_keyboard_flags.lock().expect("lock") = Some(7);
        let second = spawn_cat_agent("previewed-ctrl-s-2");
        let mut app = app_with_interactive_agent(agent);
        app.interactive_agents.push(second);
        app.focus = Focus::Preview;

        assert!(handle_focus_shortcuts(
            &mut app,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL
        ));
        assert!(app.split_picker_open);
        app.interactive_agents[0].kill();
        app.interactive_agents[1].kill();
    }

    #[test]
    fn focused_claimed_child_still_yields_reserved_shift_arrows() {
        // Shift+Up/Down stays canopy-owned inside a claimed focused session;
        // the focus gate changes who may claim, not the reserved set.
        let agent = spawn_cat_agent("focused-shift-arrows");
        *agent.kitty_keyboard_flags.lock().expect("lock") = Some(7);
        let mut app = app_with_interactive_agent(agent);
        assert!(matches!(app.focus, Focus::Agent));

        assert!(handle_focus_shortcuts(
            &mut app,
            KeyCode::Up,
            KeyModifiers::SHIFT
        ));
        assert!(handle_focus_shortcuts(
            &mut app,
            KeyCode::Down,
            KeyModifiers::SHIFT
        ));
        app.interactive_agents[0].kill();
    }
}
