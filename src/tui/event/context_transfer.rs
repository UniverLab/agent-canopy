use anyhow::Result;
use ratatui::crossterm::event::KeyCode;

use crate::tui::app::types::App;
use crate::tui::context_transfer::ContextTransferStep;

// ── Context Transfer modal ───────────────────────────────────────
//
// Step 1 (Preview):  ↑↓ / ←→ adjust capture range, Enter → Step 2, Esc → cancel.
// Step 2 (Picker):   ↑↓ navigate agents, Enter → execute, Esc → back.

/// Rebuild the payload_preview string from the current source agent state.
pub fn ctx_rebuild_preview(app: &mut App) {
    app.refresh_context_transfer_preview();
}

pub fn handle_context_transfer_key(app: &mut App, code: KeyCode) -> Result<()> {
    let Some(modal) = app.context_transfer_modal.as_ref() else {
        app.focus = crate::tui::app::types::Focus::Agent;
        return Ok(());
    };

    match modal.step {
        ContextTransferStep::Preview => match code {
            KeyCode::Esc => {
                app.close_context_transfer_modal();
            }
            KeyCode::Enter => {
                app.context_transfer_to_picker();
            }
            KeyCode::Right | KeyCode::Up | KeyCode::Char('+') => {
                let Some(history_len) = app.context_transfer_max_units() else {
                    return Ok(());
                };
                if let Some(modal) = app.context_transfer_modal.as_mut() {
                    modal.increment_field(history_len);
                }
                ctx_rebuild_preview(app);
            }
            KeyCode::Left | KeyCode::Down | KeyCode::Char('-') => {
                if let Some(modal) = app.context_transfer_modal.as_mut() {
                    modal.decrement_field();
                }
                ctx_rebuild_preview(app);
            }
            _ => {}
        },
        ContextTransferStep::AgentPicker => match code {
            KeyCode::Esc => {
                // Go back to preview step
                if let Some(modal) = app.context_transfer_modal.as_mut() {
                    modal.step = ContextTransferStep::Preview;
                }
            }
            KeyCode::Up => {
                if let Some(modal) = app.context_transfer_modal.as_mut() {
                    if modal.picker_selected > 0 {
                        modal.picker_selected -= 1;
                    }
                }
            }
            KeyCode::Down => {
                let picker_len = app.picker_interactive_entries().len();
                if let Some(modal) = app.context_transfer_modal.as_mut() {
                    if modal.picker_selected + 1 < picker_len {
                        modal.picker_selected += 1;
                    }
                }
            }
            KeyCode::Enter => {
                let dest_idx = app
                    .context_transfer_modal
                    .as_ref()
                    .map(|m| m.picker_selected)
                    .unwrap_or(0);
                app.execute_context_transfer(dest_idx);
            }
            _ => {}
        },
    }
    Ok(())
}

/// Resolve a session name to (vec_tag, index) for PTY input routing.
pub fn resolve_session(app: &App, name: &str) -> (&'static str, usize) {
    if let Some(idx) = app.interactive_agents.iter().position(|a| a.name == name) {
        return ("interactive", idx);
    }
    if let Some(idx) = app.terminal_agents.iter().position(|a| a.name == name) {
        return ("terminal", idx);
    }
    ("interactive", usize::MAX)
}
