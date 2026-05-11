use anyhow::Result;
use ratatui::crossterm::event::KeyCode;

use crate::tui::app::dialog::LaunchpadChoice;
use crate::tui::app::types::App;

pub fn handle_launchpad_key(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Esc => app.close_launchpad_dialog(),
        KeyCode::Enter => app.confirm_launchpad_dialog()?,
        _ => {
            let Some(dialog) = app.launchpad_dialog.as_mut() else {
                return Ok(());
            };
            match code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    dialog.toggle_choice();
                }
                KeyCode::Left => {
                    if dialog.selected == LaunchpadChoice::NewMission {
                        dialog.move_cursor_left();
                    } else {
                        dialog.toggle_choice();
                    }
                }
                KeyCode::Right => {
                    if dialog.selected == LaunchpadChoice::NewMission {
                        dialog.move_cursor_right();
                    } else {
                        dialog.toggle_choice();
                    }
                }
                KeyCode::Home => dialog.cursor = 0,
                KeyCode::End => dialog.cursor = dialog.new_mission.len(),
                KeyCode::Backspace => dialog.backspace(),
                KeyCode::Delete => dialog.delete(),
                KeyCode::Char(c) => dialog.insert_char(c),
                _ => {}
            }
            if !dialog.has_previous() {
                dialog.selected = LaunchpadChoice::NewMission;
            }
        }
    }

    Ok(())
}
