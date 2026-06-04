//! Event loop — polls crossterm events with a tick for data refresh.
//!
//! Navigation flow:
//!   Home (screensaver) → Preview (agent details) → Focus (log / PTY)
//!
//! Keys:
//!   Home:    ↑↓ → Preview, q quit, Esc confirm-quit, n new agent
//!   Preview: ↑↓ navigate, Enter → Focus, Esc → Home, agent actions
//!   Focus:   background → scroll log, interactive → PTY, `EscEsc` → Preview

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::time::Duration;

use crate::tui::agent::InteractiveAgent;
use crate::tui::app::types::{AgentEntry, App, Focus};
use crate::tui::app::TerminalSearch;
use crate::tui::ui;

use agent_focus::handle_agent_key;
use context_transfer::handle_context_transfer_key;
use home_preview::{handle_home_key, handle_preview_key};
use launchpad::handle_launchpad_key;
use new_agent_dialog::handle_dialog_key;
use paste::handle_paste;
use prompt_template::handle_prompt_template_key;
use rag_transfer::handle_rag_transfer_key;
use workflow_editor::handle_workflow_editor_key;

type Terminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

/// Main event loop: draw → poll events → refresh data.
pub fn run_event_loop(terminal: &mut Terminal, app: &mut App) -> Result<()> {
    while app.running {
        terminal.draw(|frame| ui::draw(frame, app))?;

        if event::poll(tick_duration(app))? {
            drain_pending_events(app)?;
        }

        app.refresh()?;
    }

    app.cleanup();
    Ok(())
}

fn tick_duration(app: &App) -> Duration {
    match app.focus {
        Focus::Agent
        | Focus::NewAgentDialog
        | Focus::LaunchpadDialog
        | Focus::KnowledgeDialog
        | Focus::ContextTransfer
        | Focus::RagTransfer
        | Focus::PromptTemplateDialog
        | Focus::WorkflowEditorDialog => Duration::from_millis(50),
        Focus::ProjectRelationDialog => Duration::from_millis(50),
        Focus::Preview => Duration::from_millis(100),
        Focus::Home if app.home_brain.is_some() => Duration::from_millis(50),
        Focus::Home => Duration::from_millis(200),
    }
}

fn drain_pending_events(app: &mut App) -> Result<()> {
    loop {
        dispatch_event(app, event::read()?)?;
        if !event::poll(Duration::from_millis(0))? {
            return Ok(());
        }
    }
}

fn dispatch_event(app: &mut App, event: Event) -> Result<()> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, key.code, key.modifiers)
        }
        Event::Mouse(mouse) => {
            app.notify_mouse_move();
            app.notify_atmosphere_mouse(mouse.column, mouse.row);
            // Suppress particles while any mouse button is held
            match mouse.kind {
                ratatui::crossterm::event::MouseEventKind::Down(
                    ratatui::crossterm::event::MouseButton::Left,
                ) => {
                    app.atmosphere_hidden = true;
                    app.atmosphere_ctx.mouse_clicked = true;
                }
                ratatui::crossterm::event::MouseEventKind::Down(_) => {
                    app.atmosphere_hidden = true;
                }
                ratatui::crossterm::event::MouseEventKind::Up(_) => {
                    app.atmosphere_hidden = false;
                }
                _ => {}
            }
            handle_mouse(app, mouse)
        }
        Event::Paste(text) => {
            handle_paste(app, &text);
            Ok(())
        }
        Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Key(_) => Ok(()),
    }
}

// ── Prompt Template Dialog ──────────────────────────────────────

mod agent_focus;
mod context_transfer;
mod home_preview;
pub(crate) mod knowledge_dialog;
mod launchpad;
mod new_agent_dialog;
mod paste;
mod prompt_template;
mod rag_transfer;
mod search_picker;
mod terminal_warp;

use knowledge_dialog::handle_knowledge_dialog_key;
mod workflow_editor;

pub fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    if dismiss_legend(app, code) || handle_global_key(app, code, modifiers) {
        return Ok(());
    }

    if app.terminal_search.is_some() {
        return handle_terminal_search_key(app, code);
    }

    dispatch_focus_key(app, code, modifiers)
}

fn dismiss_legend(app: &mut App, code: KeyCode) -> bool {
    if !app.show_legend {
        return false;
    }

    match code {
        KeyCode::Esc | KeyCode::F(1) | KeyCode::Enter => {
            app.show_legend = false;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.legend_scroll = app.legend_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let total = crate::domain::gamification::MISSIONS.len() as u16;
            let visible = 8u16;
            let max_scroll = total.saturating_sub(visible);
            app.legend_scroll = app.legend_scroll.saturating_add(1).min(max_scroll);
        }
        _ => {}
    }
    true
}

fn handle_global_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if code == KeyCode::Char('n') && modifiers.contains(KeyModifiers::CONTROL) {
        app.open_new_agent_dialog();
        return true;
    }

    if code == KeyCode::Char('b')
        && modifiers.contains(KeyModifiers::CONTROL)
        && matches!(app.focus, Focus::Agent)
        && !is_terminal_agent_selected(app)
    {
        app.open_simple_prompt_dialog(None);
        return true;
    }

    if code == KeyCode::F(2) {
        app.toggle_sidebar_mode();
        if matches!(app.focus, Focus::Agent) {
            app.focus = Focus::Preview;
        }
        return true;
    }

    if code == KeyCode::F(3) {
        app.toggle_activity_panel();
        return true;
    }

    if code == KeyCode::Char('f')
        && modifiers.contains(KeyModifiers::CONTROL)
        && matches!(app.focus, Focus::Agent)
    {
        open_terminal_search(app);
        return true;
    }

    false
}

fn dispatch_focus_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    match app.focus {
        Focus::Home => handle_home_key(app, code, modifiers),
        Focus::Preview => handle_preview_key(app, code, modifiers),
        Focus::NewAgentDialog => handle_dialog_key(app, code),
        Focus::LaunchpadDialog => handle_launchpad_key(app, code),
        Focus::KnowledgeDialog => handle_knowledge_dialog_key(app, code),
        Focus::Agent => handle_agent_key(app, code, modifiers),
        Focus::ContextTransfer => handle_context_transfer_key(app, code),
        Focus::RagTransfer => handle_rag_transfer_key(app, code),
        Focus::PromptTemplateDialog => handle_prompt_template_key(app, code, modifiers),
        Focus::WorkflowEditorDialog => handle_workflow_editor_key(app, code, modifiers),
        Focus::ProjectRelationDialog => handle_preview_key(app, code, modifiers),
    }
}

// ── Mouse: scroll wheel + Shift+Click to copy selection ─────────────

fn handle_mouse(app: &mut App, mouse: MouseEvent) -> Result<()> {
    if try_forward_mouse_to_pty(app, &mouse) || handle_copy_click(app, &mouse) {
        return Ok(());
    }

    handle_mouse_scroll(app, &mouse);
    Ok(())
}

fn handle_copy_click(app: &mut App, mouse: &MouseEvent) -> bool {
    if !matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
        return false;
    }

    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        handle_shift_click_copy(app);
        return true;
    }

    false
}

fn handle_mouse_scroll(app: &mut App, mouse: &MouseEvent) {
    let Some(dir) = scroll_direction(mouse.kind) else {
        return;
    };

    if handle_sync_panel_scroll(app, mouse, dir) {
        return;
    }

    handle_scroll(app, dir);
}

fn scroll_direction(kind: MouseEventKind) -> Option<i32> {
    match kind {
        MouseEventKind::ScrollUp => Some(1),
        MouseEventKind::ScrollDown => Some(-1),
        _ => None,
    }
}

fn handle_sync_panel_scroll(app: &mut App, mouse: &MouseEvent, dir: i32) -> bool {
    let Some(sync_area) = app.last_sync_area else {
        return false;
    };

    if !rect_contains_point(sync_area, mouse.column, mouse.row) {
        return false;
    }

    if dir > 0 {
        app.sync_scroll_offset = app.sync_scroll_offset.saturating_sub(3);
    } else {
        app.sync_scroll_offset = app.sync_scroll_offset.saturating_add(3);
    }

    true
}

fn rect_contains_point(rect: ratatui::layout::Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// Try to forward the mouse event to the focused PTY agent.
/// Returns `true` if the event was consumed.
fn try_forward_mouse_to_pty(app: &mut App, mouse: &MouseEvent) -> bool {
    let Some((pty_col, pty_row)) = mouse_pty_position(app, mouse) else {
        return false;
    };

    with_selected_terminal_like_mut(app, |agent| {
        agent
            .forward_mouse(mouse.kind, MouseButton::Left, pty_col, pty_row)
            .unwrap_or(false)
    })
    .unwrap_or(false)
}

fn mouse_pty_position(app: &App, mouse: &MouseEvent) -> Option<(u16, u16)> {
    let sidebar_width = sidebar_width(app);
    let panel_y = app.last_panel_y;
    let panel_width = app.last_panel_inner.0;
    let panel_height = app.last_panel_inner.1;

    if mouse.column < sidebar_width
        || mouse.row < panel_y
        || mouse.column >= sidebar_width.saturating_add(panel_width)
        || mouse.row >= panel_y.saturating_add(panel_height)
    {
        return None;
    }

    Some((
        mouse.column.saturating_sub(sidebar_width),
        mouse.row.saturating_sub(panel_y),
    ))
}

fn sidebar_width(app: &App) -> u16 {
    if app.sidebar_visible {
        30
    } else {
        0
    }
}

fn handle_shift_click_copy(app: &mut App) {
    mark_copied(app);

    let Some(text) =
        with_selected_terminal_like(app, InteractiveAgent::get_plain_text_from_screen).flatten()
    else {
        return;
    };

    let _ = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(&text));
}

fn mark_copied(app: &mut App) {
    app.show_copied = true;
    app.copied_at = std::time::Instant::now();
}

fn scroll_speed(app: &App) -> usize {
    let elapsed_ms = app.last_scroll_at.elapsed().as_millis();
    match elapsed_ms {
        0..=60 => 8,
        61..=120 => 4,
        121..=200 => 2,
        _ => 1,
    }
}

fn open_terminal_search(app: &mut App) {
    let Some((is_terminal, idx)) = selected_terminal_like(app) else {
        return;
    };

    app.terminal_search = Some(if is_terminal {
        TerminalSearch::new(idx)
    } else {
        TerminalSearch::new_interactive(idx)
    });
}

fn handle_scroll(app: &mut App, dir: i32) {
    match app.focus {
        Focus::Agent | Focus::Preview => {
            let speed = scroll_speed(app);
            app.last_scroll_at = std::time::Instant::now();
            scroll_focused_agent(app, dir * speed as i32);
        }
        Focus::Home => {
            if dir > 0 {
                app.select_prev();
            } else {
                app.select_next();
            }
        }
        Focus::NewAgentDialog => {
            if let Some(dialog) = &mut app.new_agent_dialog {
                let len = dialog.filtered_dir_entries().len();
                if dir > 0 && dialog.dir_selected > 0 {
                    dialog.dir_selected -= 1;
                } else if dir < 0 && dialog.dir_selected + 1 < len {
                    dialog.dir_selected += 1;
                }
            }
        }
        Focus::LaunchpadDialog
        | Focus::KnowledgeDialog
        | Focus::ContextTransfer
        | Focus::RagTransfer
        | Focus::PromptTemplateDialog
        | Focus::WorkflowEditorDialog => {}
        Focus::ProjectRelationDialog => {}
    }
}

fn scroll_focused_agent(app: &mut App, dir: i32) {
    let step = dir.unsigned_abs() as usize;

    if let Some((is_terminal, idx)) = selected_terminal_like(app) {
        let _ = with_terminal_like_agent_mut(app, is_terminal, idx, |agent| {
            scroll_terminal_like_agent(agent, dir, step);
        });
        return;
    }

    scroll_log(app, dir, step);
}

fn scroll_terminal_like_agent(agent: &mut InteractiveAgent, dir: i32, step: usize) {
    if agent.in_alternate_screen() {
        let _ = agent.forward_scroll(dir > 0);
        return;
    }

    if dir > 0 {
        let max = agent.max_scroll();
        agent.scroll_offset = (agent.scroll_offset + step).min(max);
        return;
    }

    agent.scroll_offset = agent.scroll_offset.saturating_sub(step);
}

fn scroll_log(app: &mut App, dir: i32, step: usize) {
    for _ in 0..step {
        if dir > 0 {
            app.scroll_log_up();
        } else {
            app.scroll_log_down();
        }
    }
}

fn selected_terminal_like(app: &App) -> Option<(bool, usize)> {
    match app.selected_agent()? {
        AgentEntry::Interactive(idx) => Some((false, *idx)),
        AgentEntry::Terminal(idx) => Some((true, *idx)),
        _ => None,
    }
}

fn with_selected_terminal_like<R>(app: &App, f: impl FnOnce(&InteractiveAgent) -> R) -> Option<R> {
    let (is_terminal, idx) = selected_terminal_like(app)?;
    with_terminal_like_agent(app, is_terminal, idx, f)
}

fn with_selected_terminal_like_mut<R>(
    app: &mut App,
    f: impl FnOnce(&mut InteractiveAgent) -> R,
) -> Option<R> {
    let (is_terminal, idx) = selected_terminal_like(app)?;
    with_terminal_like_agent_mut(app, is_terminal, idx, f)
}

fn with_terminal_like_agent<R>(
    app: &App,
    is_terminal: bool,
    idx: usize,
    f: impl FnOnce(&InteractiveAgent) -> R,
) -> Option<R> {
    if is_terminal {
        return app.terminal_agents.get(idx).map(f);
    }

    app.interactive_agents.get(idx).map(f)
}

fn with_terminal_like_agent_mut<R>(
    app: &mut App,
    is_terminal: bool,
    idx: usize,
    f: impl FnOnce(&mut InteractiveAgent) -> R,
) -> Option<R> {
    if is_terminal {
        return app.terminal_agents.get_mut(idx).map(f);
    }

    app.interactive_agents.get_mut(idx).map(f)
}

// ── Terminal scrollback search (Ctrl+F) ─────────────────────────────

fn handle_terminal_search_key(app: &mut App, code: KeyCode) -> Result<()> {
    let Some(mut search) = app.terminal_search.take() else {
        return Ok(());
    };

    if code == KeyCode::Esc {
        return Ok(());
    }

    match code {
        KeyCode::Enter => {
            jump_terminal_search_match(&search, app);
            search.next_match();
        }
        KeyCode::Up => {
            search.prev_match();
            jump_terminal_search_match(&search, app);
        }
        KeyCode::Down => {
            search.next_match();
            jump_terminal_search_match(&search, app);
        }
        KeyCode::Char(c) => {
            search.query.push(c);
            refresh_terminal_search(&mut search, app);
            if !search.match_rows.is_empty() {
                search.current_match = 0;
                jump_terminal_search_match(&search, app);
            }
        }
        KeyCode::Backspace => {
            search.query.pop();
            refresh_terminal_search(&mut search, app);
        }
        _ => {}
    }

    app.terminal_search = Some(search);
    Ok(())
}

fn refresh_terminal_search(search: &mut TerminalSearch, app: &App) {
    let _ = with_terminal_like_agent(app, search.is_terminal, search.agent_idx, |agent| {
        search.search(agent);
    });
}

fn jump_terminal_search_match(search: &TerminalSearch, app: &mut App) {
    let _ = with_terminal_like_agent_mut(app, search.is_terminal, search.agent_idx, |agent| {
        search.jump_to_match(agent);
    });
}

/// Check if the currently selected agent is a Terminal agent.
fn is_terminal_agent_selected(app: &App) -> bool {
    matches!(app.selected_agent(), Some(AgentEntry::Terminal(_)))
}

#[cfg(test)]
mod tests {
    use super::search_picker::resolve_cd_picker_selection;
    use crate::tui::terminal_history::{PickerMode, SuggestionItem, SuggestionPicker};
    use std::path::PathBuf;

    #[test]
    fn test_cd_picker_selection_keeps_downstream_path() {
        let picker = SuggestionPicker {
            input: "./alpha".to_string(),
            mode: PickerMode::CdDirectory,
            all_items: vec![SuggestionItem {
                text: "./beta".to_string(),
                label: "./beta".to_string(),
                count: 0,
            }],
            items: vec![SuggestionItem {
                text: "./beta".to_string(),
                label: "./beta".to_string(),
                count: 0,
            }],
            selected: 0,
            scroll_offset: 0,
            cd_base_dir: Some(PathBuf::from("/repo")),
            cd_current_dir: Some(PathBuf::from("/repo/alpha")),
        };

        let resolved = resolve_cd_picker_selection(&picker).unwrap();
        assert_eq!(resolved, "alpha/beta");
    }

    #[test]
    fn test_cd_picker_selection_keeps_parent_path_relative_to_base() {
        let picker = SuggestionPicker {
            input: "./alpha/beta".to_string(),
            mode: PickerMode::CdDirectory,
            all_items: vec![SuggestionItem {
                text: "..".to_string(),
                label: "../".to_string(),
                count: 0,
            }],
            items: vec![SuggestionItem {
                text: "..".to_string(),
                label: "../".to_string(),
                count: 0,
            }],
            selected: 0,
            scroll_offset: 0,
            cd_base_dir: Some(PathBuf::from("/repo")),
            cd_current_dir: Some(PathBuf::from("/repo/alpha/beta")),
        };

        let resolved = resolve_cd_picker_selection(&picker).unwrap();
        assert_eq!(resolved, "alpha");
    }
}
