use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::app::types::{App, LoopEditorMode, RouterField};

pub fn handle_loop_editor_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    let is_router = matches!(
        app.loop_editor_dialog.as_ref().map(|d| &d.mode),
        Some(LoopEditorMode::RouterRoutes)
    );
    if is_router {
        return handle_router_routes_key(app, code, modifiers);
    }

    match code {
        KeyCode::Esc => app.cancel_loop_editor_dialog(),
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.save_loop_editor_dialog()?;
        }
        KeyCode::Left => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.move_left();
            }
        }
        KeyCode::Right => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.move_right();
            }
        }
        KeyCode::Home => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.move_home();
            }
        }
        KeyCode::End => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.move_end();
            }
        }
        KeyCode::Backspace => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                dialog.backspace();
            }
        }
        KeyCode::Enter => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                dialog.insert_char('\n');
            }
        }
        KeyCode::Tab => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                dialog.insert_str("    ");
            }
        }
        KeyCode::Char(value) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                dialog.insert_char(value);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Key handling for the router routes editor ([`LoopEditorMode::RouterRoutes`])
/// — a structured route/fallback/wiring form, not the free-text buffer the
/// other modes share, so it gets its own key map: Up/Down moves between
/// routes, Tab/Shift+Tab between a route's label/description/target fields,
/// Left/Right cycles the target field's candidate node, and Ctrl+N/D/F
/// add/remove/mark-fallback a route (plain letters stay free for typing
/// into the label/description fields).
fn handle_router_routes_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    match code {
        KeyCode::Esc => app.cancel_loop_editor_dialog(),
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.save_loop_editor_dialog()?;
        }
        KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                dialog.router_add_route();
            }
        }
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                dialog.router_remove_route();
            }
        }
        KeyCode::Char('f') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                dialog.router_set_fallback();
            }
        }
        KeyCode::Up => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.router_move_route(false);
            }
        }
        KeyCode::Down => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.router_move_route(true);
            }
        }
        KeyCode::Tab | KeyCode::Enter => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.router_next_field();
            }
        }
        KeyCode::BackTab => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.router_prev_field();
            }
        }
        KeyCode::Left => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                if dialog.router_field == RouterField::Target {
                    dialog.cycle_router_target(false);
                }
            }
        }
        KeyCode::Right => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                if dialog.router_field == RouterField::Target {
                    dialog.cycle_router_target(true);
                }
            }
        }
        KeyCode::Backspace => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                if dialog.router_field == RouterField::Target {
                    dialog.router_clear_target();
                } else {
                    dialog.router_pop_char();
                }
            }
        }
        KeyCode::Char(value) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(dialog) = app.loop_editor_dialog.as_mut() {
                dialog.parse_error = None;
                dialog.router_push_char(value);
            }
        }
        _ => {}
    }
    Ok(())
}
