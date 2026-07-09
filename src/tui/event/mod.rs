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
use crate::tui::app::types::{AgentEntry, App, Focus, SidebarMode, TerminalSelection};
use crate::tui::app::TerminalSearch;
use crate::tui::ui;

use agent_focus::handle_agent_key;
use context_transfer::{handle_context_transfer_key, resolve_split_focused_terminal_like};
use home_preview::{handle_home_key, handle_preview_key};
use launchpad::handle_launchpad_key;
use loop_editor::handle_loop_editor_key;
use loop_form::handle_loop_form_key;
use new_agent_dialog::handle_dialog_key;
use paste::handle_paste;
use prompt_template::handle_prompt_template_key;
use rag_transfer::handle_rag_transfer_key;

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
        | Focus::LoopEditorDialog
        | Focus::LoopFormDialog => Duration::from_millis(50),
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
mod loop_editor;
mod loop_form;

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

    let unlocked_count = app.mission_manager.unlocked_count();
    let max_selected = unlocked_count.saturating_sub(1);

    match code {
        KeyCode::Esc | KeyCode::F(1) | KeyCode::Enter => {
            app.show_legend = false;
        }
        KeyCode::Up | KeyCode::Char('k') if app.legend_selected > 0 => {
            app.legend_selected -= 1;
        }
        KeyCode::Down | KeyCode::Char('j') if app.legend_selected < max_selected => {
            app.legend_selected += 1;
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
        toggle_sidebar_and_normalize_focus(app);
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

/// Shared by the F2 key and a sidebar right-click.
fn toggle_sidebar_and_normalize_focus(app: &mut App) {
    app.toggle_sidebar_mode();
    if matches!(app.focus, Focus::Agent) {
        app.focus = Focus::Preview;
    }
}

fn dispatch_focus_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    match app.focus {
        Focus::Home => handle_home_key(app, code, modifiers),
        Focus::Preview => handle_preview_key(app, code, modifiers),
        Focus::NewAgentDialog => handle_dialog_key(app, code, modifiers),
        Focus::LaunchpadDialog => handle_launchpad_key(app, code),
        Focus::KnowledgeDialog => handle_knowledge_dialog_key(app, code),
        Focus::Agent => handle_agent_key(app, code, modifiers),
        Focus::ContextTransfer => handle_context_transfer_key(app, code),
        Focus::RagTransfer => handle_rag_transfer_key(app, code),
        Focus::PromptTemplateDialog => handle_prompt_template_key(app, code, modifiers),
        Focus::LoopEditorDialog => handle_loop_editor_key(app, code, modifiers),
        Focus::LoopFormDialog => handle_loop_form_key(app, code, modifiers),
        Focus::ProjectRelationDialog => handle_preview_key(app, code, modifiers),
    }
}

// ── Mouse: scroll wheel + Shift+Click to copy selection ─────────────

fn handle_mouse(app: &mut App, mouse: MouseEvent) -> Result<()> {
    if handle_sidebar_mouse(app, &mouse) {
        return Ok(());
    }

    if try_forward_mouse_to_pty(app, &mouse) {
        // The child program owns the mouse; any pending selection is stale.
        app.terminal_selection = None;
        return Ok(());
    }

    if handle_copy_click(app, &mouse) || handle_selection_mouse(app, &mouse) {
        return Ok(());
    }

    handle_mouse_scroll(app, &mouse);
    Ok(())
}

// ── Mouse: agent sidebar (hover, click, scroll, right-click) ────────

/// Handle a mouse event landing on the agent sidebar: hover highlight,
/// left-click to select/enter an agent, scroll to page the list, and
/// right-click as a shortcut for F2. Returns `true` if the event was
/// consumed and no further mouse handling should run.
fn handle_sidebar_mouse(app: &mut App, mouse: &MouseEvent) -> bool {
    let in_sidebar = app.sidebar_visible
        && app.sidebar_mode == SidebarMode::Agents
        && mouse.column < sidebar_width(app);

    if !in_sidebar {
        if app.hovered_row.is_some() {
            app.hovered_row = None;
        }
        return false;
    }

    match mouse.kind {
        MouseEventKind::Moved => {
            app.hovered_row = sidebar_agent_at(app, mouse.row);
            true
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(idx) = sidebar_agent_at(app, mouse.row) {
                let reenter = app.selected == idx && !app.agents_rag_focused;
                app.select_agent_at(idx);
                app.focus = if reenter {
                    Focus::Agent
                } else {
                    Focus::Preview
                };
            }
            true
        }
        MouseEventKind::Down(MouseButton::Right) => {
            toggle_sidebar_and_normalize_focus(app);
            true
        }
        MouseEventKind::ScrollUp => {
            scroll_sidebar(app, 1);
            true
        }
        MouseEventKind::ScrollDown => {
            scroll_sidebar(app, -1);
            true
        }
        _ => false,
    }
}

/// Map a sidebar row (terminal row coordinate) to the agent index rendered
/// there on the last frame, via the click map populated during draw.
fn sidebar_agent_at(app: &App, row: u16) -> Option<usize> {
    app.sidebar_click_map
        .iter()
        .find(|&&(_, start, end)| row >= start && row < end)
        .map(|&(idx, _, _)| idx)
}

fn scroll_sidebar(app: &mut App, dir: i32) {
    let total = app.sidebar_click_map.len();
    let max_visible = app.sidebar_visible_capacity.max(1);
    app.sidebar_scroll_offset =
        clamp_sidebar_scroll(app.sidebar_scroll_offset, total, max_visible, dir);
}

/// Pure clamped increment/decrement for the sidebar's manual scroll offset:
/// scrolling down moves further into the list (up to the last page),
/// scrolling up retreats back toward the top.
fn clamp_sidebar_scroll(offset: usize, total_items: usize, max_visible: usize, dir: i32) -> usize {
    let max_offset = total_items.saturating_sub(max_visible);
    if dir < 0 {
        (offset + 1).min(max_offset)
    } else {
        offset.saturating_sub(1)
    }
}

fn handle_copy_click(app: &mut App, mouse: &MouseEvent) -> bool {
    if !matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
        return false;
    }

    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        app.terminal_selection = None;
        handle_shift_click_copy(app);
        return true;
    }

    false
}

// ── Mouse drag selection over the focused PTY pane ───────────────────
//
// Click+drag selects text cells linearly (like a terminal); on release the
// selection is copied to the system clipboard without any TUI decoration.
fn handle_selection_mouse(app: &mut App, mouse: &MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.terminal_selection = None;
            let Some(agent) = focused_terminal_like(app) else {
                return false;
            };
            let Some((col, row)) = mouse_pty_position(app, mouse) else {
                return false;
            };
            app.terminal_selection = Some(TerminalSelection {
                agent,
                start: (row, col),
                end: (row, col),
                dragging: true,
            });
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let (col, row) = clamped_pty_position(app, mouse);
            let Some(sel) = app.terminal_selection.as_mut() else {
                return false;
            };
            if !sel.dragging {
                return false;
            }
            sel.end = (row, col);
            true
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(sel) = app.terminal_selection.take() else {
                return false;
            };
            if !sel.dragging || sel.start == sel.end {
                return false;
            }
            let (start, end) = sel.normalized();
            let text = with_terminal_like_agent(app, sel.agent.0, sel.agent.1, |agent| {
                agent
                    .screen_snapshot()
                    .map(|snap| snap.selection_text(start, end))
            })
            .flatten()
            .unwrap_or_default();
            if text.trim().is_empty() {
                return true;
            }
            crate::tui::clipboard::set_text(&text);
            mark_copied(app);
            true
        }
        _ => false,
    }
}

/// Pane-relative (col, row) for drag events, clamped to the panel bounds so
/// dragging past an edge extends the selection to that edge. Uses the
/// focused panel's last-rendered geometry, which tracks whichever half of a
/// split (if any) is currently focused.
fn clamped_pty_position(app: &App, mouse: &MouseEvent) -> (u16, u16) {
    let (panel_w, panel_h) = app.last_panel_inner;
    let col = mouse
        .column
        .saturating_sub(app.last_panel_x)
        .min(panel_w.saturating_sub(1));
    let row = mouse
        .row
        .saturating_sub(app.last_panel_y)
        .min(panel_h.saturating_sub(1));
    (col, row)
}

fn handle_mouse_scroll(app: &mut App, mouse: &MouseEvent) {
    let Some(dir) = scroll_direction(mouse.kind) else {
        return;
    };

    // Scrolling shifts the pane content under a selection's coordinates.
    app.terminal_selection = None;

    if app.show_legend {
        let unlocked_count = app.mission_manager.unlocked_count();
        let max_selected = unlocked_count.saturating_sub(1);
        if dir > 0 {
            if app.legend_selected > 0 {
                app.legend_selected -= 1;
            }
        } else {
            if app.legend_selected < max_selected {
                app.legend_selected += 1;
            }
        }
        return;
    }

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
    let panel_x = app.last_panel_x;
    let panel_y = app.last_panel_y;
    let panel_width = app.last_panel_inner.0;
    let panel_height = app.last_panel_inner.1;

    if mouse.column < panel_x
        || mouse.row < panel_y
        || mouse.column >= panel_x.saturating_add(panel_width)
        || mouse.row >= panel_y.saturating_add(panel_height)
    {
        return None;
    }

    Some((
        mouse.column.saturating_sub(panel_x),
        mouse.row.saturating_sub(panel_y),
    ))
}

fn sidebar_width(app: &App) -> u16 {
    if app.sidebar_visible {
        crate::tui::ui::SIDEBAR_WIDTH
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

    crate::tui::clipboard::set_text(&text);
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
        | Focus::LoopEditorDialog
        | Focus::LoopFormDialog => {}
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

/// Resolve the terminal-like agent that currently owns PTY input: the
/// focused half of an active split, or the sidebar-selected agent otherwise.
fn focused_terminal_like(app: &App) -> Option<(bool, usize)> {
    if app.active_split_id.is_some() {
        return resolve_split_focused_terminal_like(app);
    }
    selected_terminal_like(app)
}

fn with_selected_terminal_like<R>(app: &App, f: impl FnOnce(&InteractiveAgent) -> R) -> Option<R> {
    let (is_terminal, idx) = focused_terminal_like(app)?;
    with_terminal_like_agent(app, is_terminal, idx, f)
}

fn with_selected_terminal_like_mut<R>(
    app: &mut App,
    f: impl FnOnce(&mut InteractiveAgent) -> R,
) -> Option<R> {
    let (is_terminal, idx) = focused_terminal_like(app)?;
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

#[cfg(test)]
mod sidebar_mouse_tests {
    use super::*;
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
            prompt: "prompt".to_string(),
            trigger: Some(Trigger::Cron {
                schedule_expr: "0 9 * * *".to_string(),
            }),
            cli: Cli::new("claude"),
            model: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: Utc::now(),
            log_path: "/tmp/test-sidebar-mouse.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    /// Builds an App with `count` background agents and a `sidebar_click_map`
    /// matching the row layout `draw_agent_list` produces (3-row cards, 1-row
    /// gap): agent `i` occupies rows `[i*4, i*4+3)`.
    fn app_with_agents(count: usize) -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = (0..count)
            .map(|i| AgentEntry::Agent(cron_agent(&format!("agent-{i}"))))
            .collect();
        app.sidebar_visible = true;
        app.sidebar_mode = SidebarMode::Agents;
        app.sidebar_click_map = (0..count)
            .map(|i| (i, (i * 4) as u16, (i * 4 + 3) as u16))
            .collect();
        app.selected = 0;
        app.focus = Focus::Preview;
        app
    }

    #[test]
    fn moved_updates_hovered_row_without_changing_selection() {
        let mut app = app_with_agents(3);
        assert_eq!(app.hovered_row, None);

        let mouse = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 5,
            row: 5, // falls in agent #1's rows [4, 7)
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_sidebar_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.hovered_row, Some(1));
        assert_eq!(app.selected, 0, "hover must not change selection");
    }

    #[test]
    fn left_click_selects_agent_under_cursor() {
        let mut app = app_with_agents(3);

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 9, // falls in agent #2's rows [8, 11)
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_sidebar_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.selected, 2);
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn left_click_on_already_selected_agent_enters_it() {
        let mut app = app_with_agents(3);
        app.selected = 2;

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 9, // agent #2 again
            modifiers: KeyModifiers::NONE,
        };
        handle_sidebar_mouse(&mut app, &mouse);

        assert_eq!(app.selected, 2);
        assert!(matches!(app.focus, Focus::Agent));
    }

    #[test]
    fn scroll_down_increments_offset_within_bounds() {
        // 20 agents, 8 rows visible per the last render.
        let mut app = app_with_agents(20);
        app.sidebar_visible_capacity = 8;
        assert_eq!(app.sidebar_scroll_offset, 0);

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_sidebar_mouse(&mut app, &mouse);

        assert_eq!(app.sidebar_scroll_offset, 1);
    }

    #[test]
    fn clamp_sidebar_scroll_never_exceeds_max_offset() {
        // total=20, max_visible=8 → max_offset=12.
        assert_eq!(clamp_sidebar_scroll(0, 20, 8, -1), 1);
        assert_eq!(clamp_sidebar_scroll(12, 20, 8, -1), 12);
        assert_eq!(clamp_sidebar_scroll(1, 20, 8, 1), 0);
        assert_eq!(clamp_sidebar_scroll(0, 20, 8, 1), 0);
    }

    #[test]
    fn right_click_behaves_like_f2() {
        let mut via_key = app_with_agents(3);
        via_key.focus = Focus::Agent;
        let key_handled = handle_global_key(&mut via_key, KeyCode::F(2), KeyModifiers::NONE);

        let mut via_click = app_with_agents(3);
        via_click.focus = Focus::Agent;
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let click_handled = handle_sidebar_mouse(&mut via_click, &mouse);

        assert!(key_handled);
        assert!(click_handled);
        assert!(via_key.sidebar_mode == via_click.sidebar_mode);
        assert!(matches!(via_key.focus, Focus::Preview));
        assert!(matches!(via_click.focus, Focus::Preview));
    }

    #[test]
    fn f2_toggles_sidebar_mode_between_agents_and_projects() {
        let mut app = app_with_agents(3);
        assert!(matches!(app.sidebar_mode, SidebarMode::Agents));

        assert!(handle_global_key(
            &mut app,
            KeyCode::F(2),
            KeyModifiers::NONE
        ));
        assert!(matches!(app.sidebar_mode, SidebarMode::Projects));

        assert!(handle_global_key(
            &mut app,
            KeyCode::F(2),
            KeyModifiers::NONE
        ));
        assert!(matches!(app.sidebar_mode, SidebarMode::Agents));
    }
}

#[cfg(test)]
mod split_selection_tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::models::{SplitGroup, SplitOrientation};
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

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

    /// Two terminal sessions in a horizontal split. Sets `last_panel_*` to
    /// pretend the *focused* half was just rendered at x=41 (the panel to
    /// the right of a 40-column left half), matching what `draw_split_panel`
    /// records for whichever side has focus.
    fn app_with_split_terminals(right_focused: bool) -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.terminal_agents.push(spawn_test_terminal("left-term"));
        app.terminal_agents.push(spawn_test_terminal("right-term"));
        app.split_groups.push(SplitGroup {
            id: "split-1".to_string(),
            orientation: SplitOrientation::Horizontal,
            session_a: "left-term".to_string(),
            session_b: "right-term".to_string(),
            created_at: Utc::now(),
        });
        app.active_split_id = Some("split-1".to_string());
        app.split_right_focused = right_focused;
        app.focus = Focus::Agent;
        app.last_panel_x = if right_focused { 41 } else { 0 };
        app.last_panel_y = 1;
        app.last_panel_inner = (39, 20);
        app
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_pty_position_uses_focused_panel_x_offset() {
        let app = app_with_split_terminals(true);

        // Inside the right panel (starts at column 41).
        assert_eq!(
            mouse_pty_position(&app, &mouse(MouseEventKind::Moved, 45, 3)),
            Some((4, 2))
        );
        // Left of the focused panel's recorded x-offset: out of bounds.
        assert_eq!(
            mouse_pty_position(&app, &mouse(MouseEventKind::Moved, 10, 3)),
            None
        );
    }

    #[test]
    fn clamped_pty_position_clamps_to_focused_panel_bounds() {
        let app = app_with_split_terminals(true);

        assert_eq!(
            clamped_pty_position(
                &app,
                &mouse(MouseEventKind::Drag(MouseButton::Left), 999, 999)
            ),
            (38, 19)
        );
    }

    #[test]
    fn focused_terminal_like_follows_split_focus_not_sidebar_selection() {
        let mut app = app_with_split_terminals(false);
        // Sidebar selection points nowhere useful (default `selected = 0`
        // with no AgentEntry list) — the split's focused panel must still
        // resolve correctly.
        assert_eq!(focused_terminal_like(&app), Some((true, 0)));

        app.split_right_focused = true;
        assert_eq!(focused_terminal_like(&app), Some((true, 1)));
    }

    #[test]
    fn drag_selection_in_split_targets_focused_right_panel() {
        let mut app = app_with_split_terminals(true);
        app.terminal_agents[1].replay_scrollback_lines(&["RIGHTPANELTEXT".to_string()]);

        let down = mouse(MouseEventKind::Down(MouseButton::Left), 41, 1);
        assert!(handle_selection_mouse(&mut app, &down));
        let sel = app.terminal_selection.as_ref().expect("selection started");
        assert_eq!(sel.agent, (true, 1));

        let drag = mouse(MouseEventKind::Drag(MouseButton::Left), 55, 1);
        assert!(handle_selection_mouse(&mut app, &drag));

        app.show_copied = false;
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 55, 1);
        assert!(handle_selection_mouse(&mut app, &up));

        assert!(app.terminal_selection.is_none());
        assert!(
            app.show_copied,
            "expected the focused (right) panel's real text to be copied"
        );
    }

    #[test]
    fn drag_selection_in_split_targets_focused_left_panel() {
        let mut app = app_with_split_terminals(false);
        app.terminal_agents[0].replay_scrollback_lines(&["LEFTPANELTEXT".to_string()]);

        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 1);
        assert!(handle_selection_mouse(&mut app, &down));
        assert_eq!(
            app.terminal_selection
                .as_ref()
                .expect("selection started")
                .agent,
            (true, 0)
        );

        let drag = mouse(MouseEventKind::Drag(MouseButton::Left), 14, 1);
        assert!(handle_selection_mouse(&mut app, &drag));

        app.show_copied = false;
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 14, 1);
        assert!(handle_selection_mouse(&mut app, &up));

        assert!(app.show_copied);
    }

    #[test]
    fn shift_click_copy_in_split_resolves_focused_panel_not_sidebar_selection() {
        let mut app = app_with_split_terminals(true);
        app.terminal_agents[1].replay_scrollback_lines(&["RIGHT SCREEN CONTENT".to_string()]);
        // `app.agents`/`app.selected` are left at their defaults (no sidebar
        // selection at all) — the old `selected_terminal_like`-based lookup
        // would resolve nothing and shift+click copy would silently no-op.

        let text = with_selected_terminal_like(&app, InteractiveAgent::get_plain_text_from_screen)
            .flatten()
            .unwrap_or_default();
        assert!(
            text.contains("RIGHT SCREEN CONTENT"),
            "expected shift+click's resolver to read the split-focused panel, got: {text:?}"
        );

        let handled = handle_copy_click(
            &mut app,
            &MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 41,
                row: 1,
                modifiers: KeyModifiers::SHIFT,
            },
        );
        assert!(handled);
    }
}
