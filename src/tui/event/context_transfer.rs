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
            KeyCode::Up | KeyCode::Down => {
                let picker_len = app.picker_interactive_entries().len();
                if picker_len == 0 {
                    return Ok(());
                }
                let forward = matches!(code, KeyCode::Down);
                if let Some(modal) = app.context_transfer_modal.as_mut() {
                    modal.picker_selected = crate::tui::selection::move_index(
                        modal.picker_selected,
                        picker_len,
                        forward,
                    );
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

/// Name of the session shown in the currently focused half of the active
/// split (session_b when the right/bottom panel is focused, session_a
/// otherwise). `None` when no split is active or the group is stale.
pub fn active_split_session_name(app: &App) -> Option<&str> {
    let split_id = app.active_split_id.as_deref()?;
    let group = app.split_groups.iter().find(|group| group.id == split_id)?;

    Some(if app.split_right_focused {
        group.session_b.as_str()
    } else {
        group.session_a.as_str()
    })
}

/// Resolve the focused terminal-like agent as `(is_terminal, idx)`, aware of
/// split layouts: when a split is active this follows the focused panel's
/// session name instead of the sidebar selection.
pub fn resolve_split_focused_terminal_like(app: &App) -> Option<(bool, usize)> {
    let name = active_split_session_name(app)?;
    let (vec, idx) = resolve_session(app, name);
    if idx == usize::MAX {
        return None;
    }
    Some((vec == "terminal", idx))
}

#[cfg(test)]
mod split_focus_tests {
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

    fn app_with_split_group(right_focused: bool) -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.split_groups.push(SplitGroup {
            id: "split-1".to_string(),
            orientation: SplitOrientation::Horizontal,
            session_a: "left-term".to_string(),
            session_b: "right-term".to_string(),
            created_at: Utc::now(),
        });
        app.active_split_id = Some("split-1".to_string());
        app.split_right_focused = right_focused;
        app
    }

    #[test]
    fn resolves_session_a_when_left_panel_focused() {
        let app = app_with_split_group(false);
        assert_eq!(active_split_session_name(&app), Some("left-term"));
    }

    #[test]
    fn resolves_session_b_when_right_panel_focused() {
        let app = app_with_split_group(true);
        assert_eq!(active_split_session_name(&app), Some("right-term"));
    }

    #[test]
    fn none_when_no_split_is_active() {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        assert_eq!(active_split_session_name(&app), None);
    }

    #[test]
    fn none_when_split_group_is_stale() {
        let mut app = app_with_split_group(false);
        app.split_groups.clear();
        assert_eq!(active_split_session_name(&app), None);
    }

    #[test]
    fn focused_terminal_like_is_none_when_named_session_has_no_agent() {
        // Neither `interactive_agents` nor `terminal_agents` has an entry
        // named "left-term" here, so resolution must report "not found"
        // instead of falling back to a bogus `usize::MAX` index.
        let app = app_with_split_group(false);
        assert_eq!(resolve_split_focused_terminal_like(&app), None);
    }
}
