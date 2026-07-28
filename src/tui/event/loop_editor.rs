use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::app::types::App;

pub fn handle_loop_editor_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
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
