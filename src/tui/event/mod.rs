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
use crate::tui::app::types::{AgentEntry, App, Focus, ProjectTab, SidebarLayer, TerminalSelection};
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
        cycle_sidebar_layer_and_normalize_focus(app);
        return true;
    }

    // Shift+←/→ walks the sidebar tab strip — one convention for both tab
    // strips in canopy (see `agent_focus::handle_project_focus_key` for the
    // project-tab side), reachable from Home, Preview, and now Focus::Agent
    // too. It used to be scoped to Home/Preview only because Focus::Agent
    // already spends Shift+←/→ on split-pane focus (see
    // `agent_focus::handle_split_panel_focus_shortcut`); the two can't both
    // hold the key at once. Resolved by context: while a split is active,
    // Shift+←/→ still means split focus — that binding is established and
    // arguably more delicate to lose since it's how you jump to the *other*
    // pane. Wherever a split isn't active, Shift+←/→ steps the sidebar tab
    // instead — matching Home/Preview and, per functional requirement 1,
    // making the strip reachable without backing out of focus first. Plain
    // ←/→ is untouched either way (loop collapse/expand on Home, forwarded
    // to the PTY / cursor movement everywhere else), and project focus /
    // playground keep opting out entirely per the existing guard below.
    if matches!(code, KeyCode::Left | KeyCode::Right)
        && modifiers.contains(KeyModifiers::SHIFT)
        && sidebar_tab_step_applies(app)
        && !app.playground_active
        && app.project_focus.is_none()
    {
        let forward = code == KeyCode::Right;
        app.step_sidebar_tab(forward);
        if app.focus == Focus::Agent && app.sidebar_layer == SidebarLayer::Knowledge {
            app.enter_project_focus(ProjectTab::Overview);
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

/// Whether Shift+←/→ should step the sidebar tab strip in the app's current
/// state: always from Home/Preview, and from Focus::Agent only when no
/// split is active (a split keeps Shift+←/→ for split-pane focus instead —
/// see the comment above this function's call site).
fn sidebar_tab_step_applies(app: &App) -> bool {
    match app.focus {
        Focus::Home | Focus::Preview => true,
        Focus::Agent => app.active_split_id.is_none(),
        _ => false,
    }
}

/// Shared by the F2 key and a sidebar right-click.
fn cycle_sidebar_layer_and_normalize_focus(app: &mut App) {
    app.cycle_sidebar_layer();
    if matches!(app.focus, Focus::Agent) {
        app.focus = Focus::Preview;
    }
}

/// A left-click on a sidebar tab label: switch to it and, like the F2/
/// right-click cycle, back out of a deep `Focus::Agent` view to Preview —
/// the newly active tab's own selection (if any) is what should be shown,
/// not whatever agent happened to be focused on the previous tab.
fn switch_sidebar_tab_and_normalize_focus(app: &mut App, layer: SidebarLayer) {
    app.switch_sidebar_tab(layer);
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
    // The prompt builder is a modal overlay: it owns the mouse while open.
    // Left-clicks on the tab bar switch the active tab; everything else is
    // swallowed so it doesn't leak to the panel/PTY underneath.
    if app.focus == Focus::PromptTemplateDialog {
        handle_prompt_dialog_mouse(app, &mouse);
        return Ok(());
    }

    if handle_sidebar_mouse(app, &mouse) {
        return Ok(());
    }

    if handle_project_panel_mouse(app, &mouse) {
        return Ok(());
    }

    if handle_loop_live_panel_mouse(app, &mouse) {
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

/// Handle a mouse event while the prompt builder is open. A left-click on one
/// of the tab-bar hit-boxes (positioned via the pure `tab_at` mapping against
/// the origin stored during the last frame) switches the active tab; entering
/// the Raw tab refreshes its composed-prompt preview. ScrollUp/ScrollDown
/// events over the Raw tab's content region scroll the preview or the editable
/// buffer without moving the cursor (view-only). All other mouse events are
/// ignored (and swallowed by the caller).
fn handle_prompt_dialog_mouse(app: &mut App, mouse: &MouseEvent) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some((x, y)) = app.prompt_tab_origin else {
                return;
            };
            let Some(tab) =
                crate::tui::app::dialog::SimplePromptDialog::tab_at(x, y, mouse.column, mouse.row)
            else {
                return;
            };

            let db = app.db.clone();
            let workdir = app.current_workdir();
            if let Some(dialog) = app.simple_prompt_dialog.as_mut() {
                let entering_raw =
                    tab == crate::tui::app::dialog::PromptTab::Raw && dialog.active_tab != tab;
                dialog.set_tab(tab);
                if entering_raw {
                    dialog.refresh_raw_preview(&db, &workdir);
                }
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let Some(dialog) = app.simple_prompt_dialog.as_ref() else {
                return;
            };
            if dialog.active_tab != crate::tui::app::dialog::PromptTab::Raw {
                return;
            }
            let Some(content_rect) = app.prompt_raw_content_rect else {
                return;
            };
            if !rect_contains_point(content_rect, mouse.column, mouse.row) {
                return;
            }
            // Skip if the @-picker overlay is open (it draws over the content area).
            if dialog.at_picker.is_some() {
                return;
            }
            let delta: isize = match mouse.kind {
                MouseEventKind::ScrollUp => -1,
                MouseEventKind::ScrollDown => 1,
                _ => unreachable!(),
            };
            let (is_empty, raw_text) = app
                .simple_prompt_dialog
                .as_ref()
                .map_or((false, String::new()), |d| {
                    (d.raw_is_empty(), d.raw_text().to_string())
                });
            let field_width = prompt_template::prompt_field_width(app);
            let dialog = app.simple_prompt_dialog.as_mut().unwrap();
            if is_empty {
                dialog.scroll_raw_preview(delta);
            } else {
                let total_lines = crate::tui::app::dialog::SimplePromptDialog::visual_line_count(
                    &raw_text,
                    field_width,
                );
                let avail_h = content_rect.height as usize;
                dialog.scroll_raw_edit(delta, total_lines, avail_h);
            }
        }
        _ => {}
    }
}

// ── Mouse: tabbed sidebar (hover, click, scroll, right-click) ───────

/// Handle a mouse event landing on the sidebar: tab-bar clicks (switch the
/// active tab), left-click to select/enter a row in the active tab's body
/// (Live/Automation agent card, Automation loop card, Knowledge project
/// row), scroll to page the active tab's list, and right-click as a
/// shortcut for F2 (cycle tab). Returns `true` if the event was consumed
/// and no further mouse handling should run.
fn handle_sidebar_mouse(app: &mut App, mouse: &MouseEvent) -> bool {
    let in_sidebar = app.sidebar_visible && mouse.column < sidebar_width(app);

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
            handle_sidebar_left_click(app, mouse.row, mouse.column);
            true
        }
        MouseEventKind::Down(MouseButton::Right) => {
            cycle_sidebar_layer_and_normalize_focus(app);
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

fn handle_sidebar_left_click(app: &mut App, row: u16, col: u16) {
    if let Some(layer) = sidebar_tab_at(app, row, col) {
        switch_sidebar_tab_and_normalize_focus(app, layer);
        return;
    }
    if let Some(idx) = sidebar_agent_at(app, row) {
        let reenter = app.selected == idx && !app.agents_rag_focused;
        app.select_agent_at(idx);
        app.focus = if reenter {
            Focus::Agent
        } else {
            Focus::Preview
        };
        return;
    }
    if let Some(loop_id) = automation_loop_at(app, row) {
        let reselect = app.sidebar_layer == SidebarLayer::Automation
            && app.selected_loop_id.as_deref() == Some(loop_id.as_str());
        app.sidebar_layer = SidebarLayer::Automation;
        app.automation_kind = crate::tui::app::AutomationKind::Loop;
        app.selected_loop_id = Some(loop_id);
        app.refresh_loops_selection();
        app.focus = Focus::Preview;
        let _ = reselect;
        return;
    }
    if let Some(idx) = project_row_at(app, row) {
        let reenter = app.sidebar_layer == SidebarLayer::Knowledge && app.selected_project == idx;
        app.sidebar_layer = SidebarLayer::Knowledge;
        app.selected_project = idx;
        app.refresh_loops_selection();
        if reenter {
            app.enter_project_focus(ProjectTab::Overview);
            app.focus = Focus::Agent;
        } else {
            app.exit_project_focus();
            app.focus = Focus::Preview;
        }
    }
}

/// Map a sidebar (row, col) to the tab cell rendered there, via the click
/// map populated during draw.
fn sidebar_tab_at(app: &App, row: u16, col: u16) -> Option<SidebarLayer> {
    app.sidebar_tab_click_map
        .iter()
        .find(|&&(_, tab_row, start, end)| row == tab_row && col >= start && col < end)
        .map(|&(layer, _, _, _)| layer)
}

/// Map a sidebar row (terminal row coordinate) to the agent index rendered
/// there on the last frame, via the click map populated during draw.
fn sidebar_agent_at(app: &App, row: u16) -> Option<usize> {
    app.sidebar_click_map
        .iter()
        .find(|&&(_, start, end)| row >= start && row < end)
        .map(|&(idx, _, _)| idx)
}

/// Map a sidebar row to the Automation-layer loop id rendered there.
fn automation_loop_at(app: &App, row: u16) -> Option<String> {
    app.automation_loop_click_map
        .iter()
        .find(|&&(_, start, end)| row >= start && row < end)
        .map(|(id, _, _)| id.clone())
}

/// Map a sidebar row to the Knowledge-layer project index rendered there.
fn project_row_at(app: &App, row: u16) -> Option<usize> {
    app.project_click_map
        .iter()
        .find(|&&(_, start, end)| row >= start && row < end)
        .map(|&(idx, _, _)| idx)
}

// ── Mouse: project Focus tab bar + tab content (main panel) ─────────

/// Handle a mouse event landing on a project's Focus tab bar/content in the
/// main panel — reuses the same click-map-populated-during-draw pattern as
/// the sidebar (functional requirement 4: no second mouse path). Returns
/// `true` if consumed.
fn handle_project_panel_mouse(app: &mut App, mouse: &MouseEvent) -> bool {
    if app.sidebar_layer != SidebarLayer::Knowledge || app.project_focus.is_none() {
        return false;
    }
    let (panel_w, panel_h) = app.last_panel_inner;
    let panel_rect =
        ratatui::layout::Rect::new(app.last_panel_x, app.last_panel_y, panel_w, panel_h);
    if !rect_contains_point(panel_rect, mouse.column, mouse.row) {
        return false;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if mouse.row == app.last_panel_y {
                if let Some(tab) = project_tab_at(app, mouse.column) {
                    app.enter_project_focus(tab);
                    return true;
                }
            }
            if let Some(idx) = project_tab_row_at(app, mouse.row) {
                app.set_project_tab_row(idx);
            }
            true
        }
        MouseEventKind::ScrollUp => {
            app.select_prev();
            true
        }
        MouseEventKind::ScrollDown => {
            app.select_next();
            true
        }
        _ => false,
    }
}

/// Handle a mouse event landing on the live loop view's spec marker strip —
/// reuses the click-map-populated-during-draw pattern from the sidebar and
/// project panel rather than a second hit-testing mechanism. A left-click on
/// a chip selects that spec (entering manual selection, same as arrow-key
/// navigation); the scroll wheel over the chip row pages a truncated strip.
/// Returns `true` if consumed.
fn handle_loop_live_panel_mouse(app: &mut App, mouse: &MouseEvent) -> bool {
    if app.focus != Focus::Preview
        || app.sidebar_layer != SidebarLayer::Automation
        || app.automation_kind != crate::tui::app::AutomationKind::Loop
    {
        return false;
    }
    let (panel_w, panel_h) = app.last_panel_inner;
    let panel_rect =
        ratatui::layout::Rect::new(app.last_panel_x, app.last_panel_y, panel_w, panel_h);
    if !rect_contains_point(panel_rect, mouse.column, mouse.row) {
        return false;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            match loop_spec_strip_at(app, mouse.row, mouse.column) {
                Some(spec_id) => {
                    app.loop_spec_strip_select(spec_id);
                    true
                }
                None => false,
            }
        }
        MouseEventKind::ScrollUp if loop_spec_strip_row(app) == Some(mouse.row) => {
            scroll_loop_spec_strip(app, 1);
            true
        }
        MouseEventKind::ScrollDown if loop_spec_strip_row(app) == Some(mouse.row) => {
            scroll_loop_spec_strip(app, -1);
            true
        }
        _ => false,
    }
}

/// Map a loop-live-panel (row, col) to the spec id whose marker chip is
/// rendered there, via the click map populated during draw.
fn loop_spec_strip_at(app: &App, row: u16, col: u16) -> Option<String> {
    app.loop_spec_strip_click_map
        .iter()
        .find(|&&(_, chip_row, start, end)| row == chip_row && col >= start && col < end)
        .map(|(id, _, _, _)| id.clone())
}

/// The screen row the marker strip's chips are rendered on, if any are
/// currently visible (empty when there's no spec queue to show).
fn loop_spec_strip_row(app: &App) -> Option<u16> {
    app.loop_spec_strip_click_map
        .first()
        .map(|&(_, row, _, _)| row)
}

fn scroll_loop_spec_strip(app: &mut App, dir: i32) {
    let total = app
        .loop_live_state
        .as_ref()
        .map_or(0, |state| state.spec_queue.len());
    let capacity = app.loop_spec_strip_capacity.max(1);
    app.loop_spec_strip_scroll =
        clamp_sidebar_scroll(app.loop_spec_strip_scroll, total, capacity, dir);
}

/// Map a main-panel column (on the tab-bar row) to the `ProjectTab` rendered
/// there on the last frame.
fn project_tab_at(app: &App, col: u16) -> Option<ProjectTab> {
    app.project_tab_click_map
        .iter()
        .find(|&&(_, start, end)| col >= start && col < end)
        .map(|&(tab, _, _)| tab)
}

/// Map a main-panel row to the active tab's list row index rendered there.
fn project_tab_row_at(app: &App, row: u16) -> Option<usize> {
    app.project_tab_row_click_map
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
        assert!(via_key.sidebar_layer == via_click.sidebar_layer);
        assert!(matches!(via_key.focus, Focus::Preview));
        assert!(matches!(via_click.focus, Focus::Preview));
    }

    #[test]
    fn f2_cycles_sidebar_layer_skipping_empty_layers() {
        let mut app = app_with_agents(3);
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Live);

        assert!(handle_global_key(
            &mut app,
            KeyCode::F(2),
            KeyModifiers::NONE
        ));
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Automation);

        // Live and Knowledge are both empty, so a second press has nowhere
        // else to land and stays on Automation.
        assert!(handle_global_key(
            &mut app,
            KeyCode::F(2),
            KeyModifiers::NONE
        ));
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Automation);
    }

    #[test]
    fn f2_cycles_through_all_three_tabs_when_all_have_content() {
        use crate::domain::models::{SplitGroup, SplitOrientation};

        // 3 background agents (Automation) from `app_with_agents`, plus a
        // split group (Live) and a project (Knowledge) so every tab has
        // something to land on and the ring wraps all the way around.
        let mut app = app_with_agents(3);
        app.agents.push(AgentEntry::Group(0));
        app.split_groups.push(SplitGroup {
            id: "g1".to_string(),
            orientation: SplitOrientation::Horizontal,
            session_a: "a".to_string(),
            session_b: "b".to_string(),
            created_at: Utc::now(),
        });
        app.projects
            .push(crate::domain::project::Project::new("/tmp/proj"));

        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Live);

        assert!(handle_global_key(
            &mut app,
            KeyCode::F(2),
            KeyModifiers::NONE
        ));
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Automation);

        assert!(handle_global_key(
            &mut app,
            KeyCode::F(2),
            KeyModifiers::NONE
        ));
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Knowledge);

        assert!(handle_global_key(
            &mut app,
            KeyCode::F(2),
            KeyModifiers::NONE
        ));
        assert_eq!(
            app.sidebar_layer,
            crate::tui::app::SidebarLayer::Live,
            "the ring wraps back to Live"
        );
    }

    #[test]
    fn shift_arrows_step_one_sidebar_tab_each_way_and_wrap() {
        let mut app = app_with_agents(3);
        app.focus = Focus::Home;
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Live);

        assert!(handle_global_key(
            &mut app,
            KeyCode::Right,
            KeyModifiers::SHIFT
        ));
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Automation);

        // Knowledge is empty here — unlike F2, the arrow still lands on it.
        assert!(handle_global_key(
            &mut app,
            KeyCode::Right,
            KeyModifiers::SHIFT
        ));
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Knowledge);

        assert!(handle_global_key(
            &mut app,
            KeyCode::Right,
            KeyModifiers::SHIFT
        ));
        assert_eq!(
            app.sidebar_layer,
            crate::tui::app::SidebarLayer::Live,
            "stepping right off the end wraps to the first tab"
        );

        assert!(handle_global_key(
            &mut app,
            KeyCode::Left,
            KeyModifiers::SHIFT
        ));
        assert_eq!(
            app.sidebar_layer,
            crate::tui::app::SidebarLayer::Knowledge,
            "stepping left off the start wraps to the last tab"
        );
    }

    #[test]
    fn unshifted_arrows_do_not_switch_sidebar_tabs() {
        // Plain ←/→ belongs to the loop expand/collapse handler; only the
        // shifted pair is ours.
        let mut app = app_with_agents(3);
        app.focus = Focus::Home;
        assert!(!handle_global_key(
            &mut app,
            KeyCode::Right,
            KeyModifiers::NONE
        ));
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Live);
    }

    #[test]
    fn shift_arrows_step_sidebar_tab_inside_a_focused_agent_without_a_split() {
        // Functional requirement 1: the sidebar tab strip must be reachable
        // from focus, not only from Home/Preview, as long as nothing else
        // (a split) already owns Shift+←/→ there.
        let mut app = app_with_agents(3);
        app.focus = Focus::Agent;
        app.selected = 0;

        assert!(handle_global_key(
            &mut app,
            KeyCode::Right,
            KeyModifiers::SHIFT
        ));

        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Automation);
        assert!(
            matches!(app.focus, Focus::Agent),
            "stepping tabs from focus must not disturb what is focused"
        );
    }

    #[test]
    fn shift_arrows_defer_to_split_focus_when_a_split_is_active() {
        // With a split active, Shift+←/→ keeps meaning split-pane focus —
        // the established binding that would otherwise be shadowed by
        // widening the sidebar-tab-step scope into Focus::Agent.
        let mut app = app_with_agents(3);
        app.focus = Focus::Agent;
        app.active_split_id = Some("split-1".to_string());

        assert!(!handle_global_key(
            &mut app,
            KeyCode::Right,
            KeyModifiers::SHIFT
        ));
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Live);
    }

    #[test]
    fn clicking_a_sidebar_tab_switches_it() {
        let mut app = app_with_agents(3);
        app.sidebar_tab_click_map = vec![
            (crate::tui::app::SidebarLayer::Live, 0, 0, 11),
            (crate::tui::app::SidebarLayer::Automation, 0, 11, 22),
            (crate::tui::app::SidebarLayer::Knowledge, 0, 22, 33),
        ];
        app.focus = Focus::Agent;

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 15, // inside the Automation cell [11, 22)
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_sidebar_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Automation);
        assert!(
            matches!(app.focus, Focus::Preview),
            "clicking a tab must back out of a deep Focus::Agent view"
        );
    }

    #[test]
    fn clicking_an_empty_sidebar_tab_still_switches_to_it() {
        // Knowledge has no projects — a keyboard F2 cycle would skip it, but
        // a deliberate click on its label must still land there so the
        // empty state ("No registered projects") is reachable.
        let mut app = app_with_agents(3);
        app.sidebar_tab_click_map = vec![
            (crate::tui::app::SidebarLayer::Live, 0, 0, 11),
            (crate::tui::app::SidebarLayer::Automation, 0, 11, 22),
            (crate::tui::app::SidebarLayer::Knowledge, 0, 22, 33),
        ];

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        handle_sidebar_mouse(&mut app, &mouse);

        assert_eq!(app.sidebar_layer, crate::tui::app::SidebarLayer::Knowledge);
    }
}

/// Functional requirement 4's mouse surface: the project Focus tab bar and
/// per-tab list rows in the main panel, reusing the same
/// click-map-populated-during-draw pattern as the sidebar.
#[cfg(test)]
mod project_panel_mouse_tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::loops::{LoopSpec, LoopSpecStatus};
    use crate::domain::project::Project;
    use crate::tui::app::types::ProjectTab;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn backlog_spec(id: &str, name: &str) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: name.to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    /// Builds an App focused on a single project's Backlog tab, with the
    /// main panel occupying rows `[2, 22)` / columns `[0, 40)` — matching
    /// what `draw_project_tabs_panel` would have set on the last frame —
    /// and a tab bar + row click map for 3 backlog specs at rows `[3, 6)`.
    fn app_with_project_backlog(count: usize) -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.projects = vec![Project::new("/tmp/project")];
        app.sidebar_layer = crate::tui::app::SidebarLayer::Knowledge;
        app.selected_project = 0;
        app.project_focus = Some(ProjectTab::Backlog);
        app.backlog_specs = (0..count)
            .map(|i| backlog_spec(&format!("spec-{i}"), &format!("Spec {i}")))
            .collect();
        app.selected_backlog = 0;
        app.focus = Focus::Agent;

        app.last_panel_x = 0;
        app.last_panel_y = 2;
        app.last_panel_inner = (40, 20);
        app.project_tab_click_map = vec![
            (ProjectTab::Overview, 0, 9),
            (ProjectTab::Backlog, 9, 17),
            (ProjectTab::Knowledge, 17, 27),
            (ProjectTab::History, 27, 35),
        ];
        app.project_tab_row_click_map = (0..count)
            .map(|i| (i, (3 + i) as u16, (4 + i) as u16))
            .collect();
        app
    }

    #[test]
    fn clicking_tab_bar_switches_active_tab() {
        let mut app = app_with_project_backlog(3);
        assert_eq!(app.project_focus, Some(ProjectTab::Backlog));

        // Row == last_panel_y is the tab-bar row; column 20 falls inside the
        // Knowledge tab's span [17, 27).
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 20,
            row: app.last_panel_y,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_project_panel_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.project_focus, Some(ProjectTab::Knowledge));
    }

    #[test]
    fn clicking_a_list_row_selects_it_without_changing_tab() {
        let mut app = app_with_project_backlog(3);

        // Row 5 falls in spec #2's span [5, 6) from `project_tab_row_click_map`.
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_project_panel_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.selected_backlog, 2);
        assert_eq!(
            app.project_focus,
            Some(ProjectTab::Backlog),
            "clicking a row must not cycle the tab"
        );
    }

    #[test]
    fn scroll_down_in_panel_advances_the_active_tabs_list() {
        let mut app = app_with_project_backlog(3);
        assert_eq!(app.selected_backlog, 0);

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_project_panel_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.selected_backlog, 1);
    }

    #[test]
    fn scroll_up_in_panel_retreats_the_active_tabs_list() {
        let mut app = app_with_project_backlog(3);
        app.selected_backlog = 1;

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_project_panel_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.selected_backlog, 0);
    }

    #[test]
    fn click_outside_the_panel_rect_is_not_consumed() {
        let mut app = app_with_project_backlog(3);

        // Row 30 is past last_panel_y (2) + last_panel_inner.1 (20) = 22.
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 30,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_project_panel_mouse(&mut app, &mouse);

        assert!(!consumed);
        assert_eq!(app.selected_backlog, 0);
        assert_eq!(app.project_focus, Some(ProjectTab::Backlog));
    }

    #[test]
    fn click_is_not_consumed_when_no_project_is_focused() {
        let mut app = app_with_project_backlog(3);
        app.project_focus = None;

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 20,
            row: app.last_panel_y,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_project_panel_mouse(&mut app, &mouse);

        assert!(!consumed);
    }

    #[test]
    fn click_is_not_consumed_outside_the_knowledge_sidebar_layer() {
        let mut app = app_with_project_backlog(3);
        app.sidebar_layer = crate::tui::app::SidebarLayer::Live;

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 20,
            row: app.last_panel_y,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_project_panel_mouse(&mut app, &mouse);

        assert!(!consumed);
        assert_eq!(
            app.project_focus,
            Some(ProjectTab::Backlog),
            "leaving Knowledge must not itself change the remembered tab"
        );
    }
}

#[cfg(test)]
mod loop_live_panel_mouse_tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::loops::{LoopSpecStatus, LoopStatus};
    use crate::tui::app::loop_live_state::{LoopLiveState, SpecQueueEntry};
    use crate::tui::app::{AutomationKind, LoopLiveFocus};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn spec_entry(id: &str, status: LoopSpecStatus) -> SpecQueueEntry {
        SpecQueueEntry {
            spec_id: id.to_string(),
            spec_name: format!("Spec {id}"),
            status,
            failure_reason: None,
        }
    }

    /// Builds an App on the live loop view with a 3-spec queue and a marker
    /// strip click map matching what `draw_loop_live_view` would have
    /// produced for chips at columns `[0,3)`, `[4,7)`, `[8,11)` on row 5.
    fn app_on_loop_live_view() -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.automation_kind = AutomationKind::Loop;
        app.loop_live_state = Some(LoopLiveState {
            loop_id: "lp1".to_string(),
            loop_name: "loop".to_string(),
            loop_status: LoopStatus::Running,
            workdir: "/tmp".to_string(),
            trigger_type: "manual".to_string(),
            schedule_expr: None,
            watch_path: None,
            autorun_at: None,
            spec_queue: vec![
                spec_entry("s1", LoopSpecStatus::Completed),
                spec_entry("s2", LoopSpecStatus::Running),
                spec_entry("s3", LoopSpecStatus::Failed),
            ],
            done_count: 1,
            total_count: 3,
            current_spec_id: Some("s2".to_string()),
            effective_nodes: Vec::new(),
            effective_edges: Vec::new(),
            ensembles: Vec::new(),
            router_taken_routes: HashMap::new(),
            current_node_id: None,
            current_node_status: None,
            current_node_started_at: None,
            current_node_iteration: None,
            current_node_output_tail: None,
        });

        app.last_panel_x = 0;
        app.last_panel_y = 2;
        app.last_panel_inner = (40, 20);
        app.loop_spec_strip_click_map = vec![
            ("s1".to_string(), 5, 0, 3),
            ("s2".to_string(), 5, 4, 7),
            ("s3".to_string(), 5, 8, 11),
        ];
        app.loop_spec_strip_capacity = 3;
        app
    }

    #[test]
    fn clicking_a_marker_selects_that_spec_and_focuses_the_strip() {
        let mut app = app_on_loop_live_view();
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 9,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_loop_live_panel_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.loop_spec_strip_selected.as_deref(), Some("s3"));
        assert_eq!(app.loop_live_focus, LoopLiveFocus::SpecStrip);
    }

    #[test]
    fn clicking_the_gap_between_chips_is_not_consumed() {
        let mut app = app_on_loop_live_view();
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_loop_live_panel_mouse(&mut app, &mouse);

        assert!(!consumed);
        assert!(app.loop_spec_strip_selected.is_none());
    }

    #[test]
    fn scroll_over_the_marker_row_pages_the_strip() {
        let mut app = app_on_loop_live_view();
        app.loop_spec_strip_capacity = 1; // force scrolling to matter
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_loop_live_panel_mouse(&mut app, &mouse);

        assert!(consumed);
        assert_eq!(app.loop_spec_strip_scroll, 1);
    }

    #[test]
    fn mouse_ignored_when_not_on_the_loop_live_view() {
        let mut app = app_on_loop_live_view();
        app.automation_kind = AutomationKind::Agent;
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 9,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let consumed = handle_loop_live_panel_mouse(&mut app, &mouse);

        assert!(!consumed);
        assert!(app.loop_spec_strip_selected.is_none());
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

#[cfg(test)]
mod tick_duration_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn app_for_tick(focus: Focus) -> App {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, dir.path()).unwrap();
        app.focus = focus;
        app
    }

    #[test]
    fn agent_focus_returns_50ms() {
        let app = app_for_tick(Focus::Agent);
        assert_eq!(tick_duration(&app), Duration::from_millis(50));
    }

    #[test]
    fn preview_focus_returns_100ms() {
        let app = app_for_tick(Focus::Preview);
        assert_eq!(tick_duration(&app), Duration::from_millis(100));
    }

    #[test]
    fn home_without_brain_returns_200ms() {
        let app = app_for_tick(Focus::Home);
        assert_eq!(tick_duration(&app), Duration::from_millis(50));
    }

    #[test]
    fn new_agent_dialog_returns_50ms() {
        let app = app_for_tick(Focus::NewAgentDialog);
        assert_eq!(tick_duration(&app), Duration::from_millis(50));
    }

    #[test]
    fn prompt_template_dialog_returns_50ms() {
        let app = app_for_tick(Focus::PromptTemplateDialog);
        assert_eq!(tick_duration(&app), Duration::from_millis(50));
    }
}

#[cfg(test)]
mod scroll_direction_tests {
    use super::*;

    #[test]
    fn scroll_up_returns_positive() {
        assert_eq!(scroll_direction(MouseEventKind::ScrollUp), Some(1));
    }

    #[test]
    fn scroll_down_returns_negative() {
        assert_eq!(scroll_direction(MouseEventKind::ScrollDown), Some(-1));
    }

    #[test]
    fn other_mouse_kinds_return_none() {
        assert!(scroll_direction(MouseEventKind::Moved).is_none());
        assert!(scroll_direction(MouseEventKind::Down(MouseButton::Left)).is_none());
        assert!(scroll_direction(MouseEventKind::Up(MouseButton::Left)).is_none());
        assert!(scroll_direction(MouseEventKind::Drag(MouseButton::Left)).is_none());
    }
}

#[cfg(test)]
mod rect_contains_point_tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn point_inside_rect() {
        let rect = Rect::new(5, 10, 20, 10);
        assert!(rect_contains_point(rect, 10, 12));
        assert!(rect_contains_point(rect, 5, 10)); // top-left corner
        assert!(rect_contains_point(rect, 24, 19)); // bottom-right corner
    }

    #[test]
    fn point_outside_rect() {
        let rect = Rect::new(5, 10, 20, 10);
        assert!(!rect_contains_point(rect, 4, 10)); // left
        assert!(!rect_contains_point(rect, 25, 10)); // right edge
        assert!(!rect_contains_point(rect, 10, 9)); // above
        assert!(!rect_contains_point(rect, 10, 20)); // below
    }

    #[test]
    fn zero_size_rect() {
        let rect = Rect::new(5, 10, 0, 0);
        assert!(!rect_contains_point(rect, 5, 10));
    }
}

#[cfg(test)]
mod clamp_sidebar_scroll_tests {
    use super::*;

    #[test]
    fn scroll_down_increments() {
        assert_eq!(clamp_sidebar_scroll(0, 20, 8, -1), 1);
    }

    #[test]
    fn scroll_down_clamps_at_max() {
        // total=20, max_visible=8 → max_offset=12
        assert_eq!(clamp_sidebar_scroll(12, 20, 8, -1), 12);
        assert_eq!(
            clamp_sidebar_scroll(13, 20, 8, -1),
            12,
            "should not exceed max"
        );
    }

    #[test]
    fn scroll_up_decrements() {
        assert_eq!(clamp_sidebar_scroll(5, 20, 8, 1), 4);
    }

    #[test]
    fn scroll_up_clamps_at_zero() {
        assert_eq!(clamp_sidebar_scroll(0, 20, 8, 1), 0);
    }

    #[test]
    fn empty_list_stays_at_zero() {
        assert_eq!(clamp_sidebar_scroll(0, 0, 8, -1), 0);
        assert_eq!(clamp_sidebar_scroll(0, 0, 8, 1), 0);
    }
}

#[cfg(test)]
mod sidebar_tab_at_tests {
    use super::*;
    use std::sync::Arc;

    fn make_app() -> App {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let dir = tempfile::tempdir().unwrap();
        App::new(db, dir.path()).unwrap()
    }

    #[test]
    fn finds_tab_by_position() {
        let mut app = make_app();
        app.sidebar_tab_click_map = vec![
            (SidebarLayer::Live, 0, 0, 11),
            (SidebarLayer::Automation, 0, 11, 22),
            (SidebarLayer::Knowledge, 0, 22, 33),
        ];

        assert_eq!(sidebar_tab_at(&app, 0, 5), Some(SidebarLayer::Live));
        assert_eq!(sidebar_tab_at(&app, 0, 15), Some(SidebarLayer::Automation));
        assert_eq!(sidebar_tab_at(&app, 0, 25), Some(SidebarLayer::Knowledge));
    }

    #[test]
    fn wrong_row_returns_none() {
        let mut app = make_app();
        app.sidebar_tab_click_map = vec![(SidebarLayer::Live, 0, 0, 11)];

        assert!(sidebar_tab_at(&app, 1, 5).is_none());
    }
}

#[cfg(test)]
mod sidebar_agent_at_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn finds_agent_by_row() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, dir.path()).unwrap();
        app.sidebar_click_map = vec![(0, 0, 3), (1, 4, 7), (2, 8, 11)];

        assert_eq!(sidebar_agent_at(&app, 1), Some(0));
        assert_eq!(sidebar_agent_at(&app, 5), Some(1));
        assert_eq!(sidebar_agent_at(&app, 9), Some(2));
        assert!(sidebar_agent_at(&app, 3).is_none());
    }
}

#[cfg(test)]
mod is_terminal_agent_selected_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn no_selection_returns_false() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(db, dir.path()).unwrap();
        assert!(!is_terminal_agent_selected(&app));
    }
}

#[cfg(test)]
mod mouse_pty_position_tests {
    use super::*;
    use std::sync::Arc;

    fn mouse_at(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn make_app() -> App {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let dir = tempfile::tempdir().unwrap();
        App::new(db, dir.path()).unwrap()
    }

    #[test]
    fn inside_panel_returns_relative_coords() {
        let mut app = make_app();
        app.last_panel_x = 10;
        app.last_panel_y = 5;
        app.last_panel_inner = (40, 20);

        assert_eq!(mouse_pty_position(&app, &mouse_at(15, 8)), Some((5, 3)));
    }

    #[test]
    fn outside_panel_returns_none() {
        let mut app = make_app();
        app.last_panel_x = 10;
        app.last_panel_y = 5;
        app.last_panel_inner = (40, 20);

        assert!(mouse_pty_position(&app, &mouse_at(3, 8)).is_none());
        assert!(mouse_pty_position(&app, &mouse_at(55, 8)).is_none());
        assert!(mouse_pty_position(&app, &mouse_at(15, 3)).is_none());
        assert!(mouse_pty_position(&app, &mouse_at(15, 30)).is_none());
    }
}

#[cfg(test)]
mod clamped_pty_position_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn clamps_to_panel_bounds() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, dir.path()).unwrap();
        app.last_panel_x = 10;
        app.last_panel_y = 5;
        app.last_panel_inner = (40, 20);

        let mouse = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 999,
            row: 999,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(clamped_pty_position(&app, &mouse), (39, 19));
    }
}

// C6: `scroll_terminal_like_agent` is the mouse-wheel routing gate that
// decides whether a scroll tick moves canopy's own buffer or goes to the
// child. Spawns a real (harmless) `cat` child so `in_alternate_screen()`
// reflects genuine vt100 state rather than a mock.
#[cfg(test)]
mod scroll_terminal_like_agent_tests {
    use super::*;
    use crate::domain::models::Cli;
    use ratatui::style::Color;

    fn spawn_cat_agent() -> InteractiveAgent {
        InteractiveAgent::spawn(
            Cli::new("cat"),
            ".",
            80,
            24,
            None,
            None,
            Color::Reset,
            Some("scroll-test-agent"),
            &[],
            None,
            None,
            None,
        )
        .expect("spawn cat as a stand-in interactive child")
    }

    #[test]
    fn outside_alternate_screen_scrolls_canopy_own_buffer() {
        let mut agent = spawn_cat_agent();
        assert!(!agent.in_alternate_screen());
        agent.scroll_offset = 5;

        scroll_terminal_like_agent(&mut agent, -1, 3);

        // Routed locally: scroll_offset moved, no PTY write was needed.
        assert_eq!(agent.scroll_offset, 2);
        agent.kill();
    }

    #[test]
    fn inside_alternate_screen_defers_to_the_child_instead() {
        let mut agent = spawn_cat_agent();
        // Feed the alternate-screen DECSET directly into the vt100 parser,
        // as a real full-screen child's own output would (rather than
        // relying on pty echo, which `cat` may or may not perform).
        agent.vt.lock().expect("vt lock").process(b"\x1b[?1049h");
        assert!(agent.in_alternate_screen());
        agent.scroll_offset = 5;

        scroll_terminal_like_agent(&mut agent, -1, 3);

        // Routed to the child: local scroll_offset is untouched.
        assert_eq!(agent.scroll_offset, 5);
        agent.kill();
    }
}
