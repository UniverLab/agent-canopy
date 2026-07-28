use anyhow::Result;
use ratatui::crossterm::event::KeyCode;
use std::path::PathBuf;

use crate::tui::app::types::{AgentEntry, App};

// ── Suggestion picker (terminal Tab autocomplete) ───────────────────

/// Handle keys while the terminal suggestion picker is visible.
pub fn handle_suggestion_picker_key(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Down => {
            if let Some(picker) = app.suggestion_picker.as_mut() {
                picker.move_down();
            }
        }
        KeyCode::Up => {
            if let Some(picker) = app.suggestion_picker.as_mut() {
                picker.move_up();
            }
        }
        KeyCode::Right => {
            let focused_name = app.focused_agent_name();
            let base_cwd = app
                .terminal_agents
                .iter()
                .find(|a| a.name == focused_name)
                .map(|a| a.working_dir.clone())
                .unwrap_or_default();
            if let Some(picker) = app.suggestion_picker.as_mut() {
                if picker.mode == crate::tui::terminal_history::PickerMode::CdDirectory {
                    let _ = picker.navigate_into(&base_cwd);
                }
            }
        }
        KeyCode::Left => {
            let focused_name = app.focused_agent_name();
            let base_cwd = app
                .terminal_agents
                .iter()
                .find(|a| a.name == focused_name)
                .map(|a| a.working_dir.clone())
                .unwrap_or_default();
            if let Some(picker) = app.suggestion_picker.as_mut() {
                if picker.mode == crate::tui::terminal_history::PickerMode::CdDirectory {
                    let _ = picker.navigate_parent(&base_cwd);
                }
            }
        }
        KeyCode::Enter => {
            let resolved = app.suggestion_picker.as_ref().and_then(|p| {
                if p.mode != crate::tui::terminal_history::PickerMode::CdDirectory {
                    return p.selected_text().map(|t| (t.to_string(), false));
                }
                resolve_cd_picker_selection(p).map(|text| (text, true))
            });
            app.suggestion_picker = None;

            if let Some((text, is_cd)) = resolved {
                insert_suggestion_into_terminal(app, &text, is_cd);
            }
        }
        KeyCode::Esc | KeyCode::Tab => {
            app.suggestion_picker = None;
        }
        KeyCode::Backspace => {
            if let Some(picker) = app.suggestion_picker.as_mut() {
                if picker.mode == crate::tui::terminal_history::PickerMode::CommandHistory {
                    picker.input.pop();
                    let filter = picker.input.clone();
                    picker.apply_filter(&filter);
                }
            }
        }
        KeyCode::Char(c) => {
            if let Some(picker) = app.suggestion_picker.as_mut() {
                if picker.mode == crate::tui::terminal_history::PickerMode::CommandHistory {
                    picker.input.push(c);
                    let filter = picker.input.clone();
                    picker.apply_filter(&filter);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn resolve_cd_picker_selection(
    picker: &crate::tui::terminal_history::SuggestionPicker,
) -> Option<String> {
    let selected = picker.selected_text()?;
    let cd_dir = picker.cd_current_dir.as_ref()?;
    let base_dir = picker.cd_base_dir.as_ref()?;

    let absolute_target = if selected == ".." {
        cd_dir.parent()?.to_path_buf()
    } else if let Some(stripped) = selected.strip_prefix("./") {
        cd_dir.join(stripped)
    } else {
        PathBuf::from(selected)
    };

    let relative = pathdiff::diff_paths(&absolute_target, base_dir).unwrap_or(absolute_target);
    let text = relative.to_string_lossy().to_string();
    if text.is_empty() {
        Some(".".to_string())
    } else {
        Some(text)
    }
}

/// Resolve a cd target path relative to a current directory.
pub fn resolve_cd_path(current_dir: &str, target: &str) -> Option<PathBuf> {
    let current = PathBuf::from(current_dir);
    let target_path = if target == ".." {
        current
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| current)
    } else if target.starts_with("../") {
        let mut path = current;
        let parts: Vec<&str> = target.split('/').collect();
        let mut parent_count = 0;
        for part in &parts {
            if *part == ".." {
                parent_count += 1;
            } else {
                break;
            }
        }
        for _ in 0..parent_count {
            if let Some(parent) = path.parent() {
                path = parent.to_path_buf();
            } else {
                break;
            }
        }
        if parts.len() > parent_count {
            for part in parts.iter().skip(parent_count) {
                if !part.is_empty() {
                    path = path.join(part);
                }
            }
        }
        path
    } else {
        current.join(target)
    };
    target_path.canonicalize().ok()
}

/// Insert the selected suggestion into the terminal's input.
pub fn insert_suggestion_into_terminal(app: &mut App, text: &str, is_cd: bool) {
    let term_idx = find_focused_terminal(app);
    let Some(idx) = term_idx else { return };

    let full_text = if is_cd {
        format!("cd {text}")
    } else {
        text.to_string()
    };

    let Some(agent) = app.terminal_agents.get_mut(idx) else {
        return;
    };

    // If this is a CD command, update the working directory
    if is_cd {
        if let Some(abs_path) = resolve_cd_path(&agent.working_dir, text) {
            agent.update_working_dir(&abs_path.to_string_lossy());
        }
    }

    if agent.warp_mode {
        // Warp mode: only update the input buffer (PTY has nothing typed yet)
        if let Ok(mut buf) = agent.input_buffer.lock() {
            buf.clear();
            buf.push_str(&full_text);
        }
        agent.warp_cursor = full_text.len();
        agent.warp_passthrough = false;
    } else {
        // Non-warp: clear PTY line with Ctrl+U then type suggestion
        let mut bytes: Vec<u8> = vec![0x15]; // Ctrl+U
        bytes.extend(full_text.as_bytes());
        let _ = agent.write_to_pty(&bytes);
        if let Ok(mut buf) = agent.input_buffer.lock() {
            buf.clear();
            buf.push_str(&full_text);
        }
    }
}

/// Find the index of the terminal agent that currently has focus.
pub fn find_focused_terminal(app: &App) -> Option<usize> {
    if let Some(ref split_id) = app.active_split_id {
        let name = app
            .split_groups
            .iter()
            .find(|g| g.id == *split_id)
            .map(|g| {
                if app.split_right_focused {
                    &g.session_b
                } else {
                    &g.session_a
                }
            })?;
        app.terminal_agents.iter().position(|a| &a.name == name)
    } else {
        match app.selected_agent() {
            Some(AgentEntry::Terminal(idx)) => {
                let idx = *idx;
                if idx < app.terminal_agents.len() {
                    Some(idx)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}
