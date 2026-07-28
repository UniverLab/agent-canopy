use anyhow::Result;
use ratatui::crossterm::event::KeyCode;

use crate::tui::app::types::App;

pub fn handle_launchpad_key(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Esc => app.close_launchpad_dialog(),
        KeyCode::Enter => {
            let can_confirm = if let Some(dialog) = app.launchpad_dialog.as_mut() {
                if dialog.can_confirm_selection() {
                    dialog.clear_submit_blocked();
                    true
                } else {
                    dialog.mark_submit_blocked();
                    false
                }
            } else {
                false
            };
            if can_confirm {
                app.confirm_launchpad_dialog()?;
            }
        }
        _ => {
            let Some(dialog) = app.launchpad_dialog.as_mut() else {
                return Ok(());
            };
            match code {
                KeyCode::Up | KeyCode::BackTab => {
                    dialog.move_selection_up();
                }
                KeyCode::Down | KeyCode::Tab => {
                    dialog.move_selection_down();
                }
                KeyCode::Left => {
                    if dialog.is_new_mission_selected() {
                        dialog.move_cursor_left();
                    }
                }
                KeyCode::Right => {
                    if dialog.is_new_mission_selected() {
                        dialog.move_cursor_right();
                    }
                }
                KeyCode::Home => {
                    if dialog.is_new_mission_selected() {
                        dialog.cursor = 0;
                    }
                }
                KeyCode::End => {
                    if dialog.is_new_mission_selected() {
                        dialog.cursor = dialog.new_mission.len();
                    }
                }
                KeyCode::Backspace => dialog.backspace(),
                KeyCode::Delete => dialog.delete(),
                KeyCode::Char(c) => dialog.insert_char(c),
                _ => {}
            }
        }
    }

    Ok(())
}
