use anyhow::Result;
use ratatui::crossterm::event::KeyCode;

use crate::tui::app::types::App;

pub fn handle_rag_transfer_key(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Esc => {
            app.close_rag_transfer_modal();
        }
        KeyCode::Up => {
            if let Some(modal) = app.rag_transfer_modal.as_mut() {
                if modal.picker_selected > 0 {
                    modal.picker_selected -= 1;
                }
            }
        }
        KeyCode::Down => {
            let picker_len = app.picker_interactive_entries().len();
            if let Some(modal) = app.rag_transfer_modal.as_mut() {
                if modal.picker_selected + 1 < picker_len {
                    modal.picker_selected += 1;
                }
            }
        }
        KeyCode::Enter => {
            let dest_idx = app
                .rag_transfer_modal
                .as_ref()
                .map(|modal| modal.picker_selected)
                .unwrap_or(0);
            app.execute_rag_transfer(dest_idx);
        }
        _ => {}
    }
    Ok(())
}
