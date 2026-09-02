use anyhow::Result;
use ratatui::crossterm::event::KeyCode;

use crate::tui::app::types::App;

pub fn handle_rag_transfer_key(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Esc => {
            app.close_rag_transfer_modal();
        }
        KeyCode::Up | KeyCode::Down => {
            let picker_len = app.picker_interactive_entries().len();
            if picker_len == 0 {
                return Ok(());
            }
            let forward = matches!(code, KeyCode::Down);
            if let Some(modal) = app.rag_transfer_modal.as_mut() {
                modal.picker_selected =
                    crate::tui::selection::move_index(modal.picker_selected, picker_len, forward);
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
