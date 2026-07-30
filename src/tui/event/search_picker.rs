use anyhow::Result;
use ratatui::crossterm::event::KeyCode;
use std::path::PathBuf;

use super::terminal_warp::{record_terminal_command, submit_warp_input};
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

        // A cd picker selection is already a confirmed choice: run it
        // immediately instead of leaving it in the input box for a second Enter.
        if is_cd {
            submit_warp_input(app, idx);
        }
    } else {
        // Non-warp: clear PTY line with Ctrl+U then type suggestion
        let mut bytes: Vec<u8> = vec![0x15]; // Ctrl+U
        bytes.extend(full_text.as_bytes());
        let _ = agent.write_to_pty(&bytes);
        if let Ok(mut buf) = agent.input_buffer.lock() {
            buf.clear();
            buf.push_str(&full_text);
        }

        if is_cd {
            // Submit exactly as a real Enter keypress would: send the
            // terminating CR and mirror the shadow-buffer bookkeeping that
            // the raw passthrough path performs for a real Enter key.
            let _ = agent.write_to_pty(b"\r");
            record_terminal_command(app, idx, &full_text);
            if let Ok(mut buf) = app.terminal_agents[idx].input_buffer.lock() {
                buf.clear();
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::tui::agent::InteractiveAgent;
    use crate::tui::terminal_history::{PickerMode, SessionHistory, SuggestionPicker};
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    /// `cat` is a stand-in shell: it never enters the alternate screen or
    /// looks like a sensitive prompt, so it exercises the same code paths a
    /// real shell would for the purposes of this suite.
    fn spawn_test_terminal(name: &str, cwd: &str) -> InteractiveAgent {
        InteractiveAgent::spawn_terminal(
            "cat",
            cwd,
            80,
            24,
            Some(name),
            &[],
            ratatui::style::Color::White,
        )
        .expect("spawn terminal")
    }

    fn app_with_focused_terminal(agent: InteractiveAgent) -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.terminal_agents.push(agent);
        app.agents = vec![AgentEntry::Terminal(0)];
        app.selected = 0;
        app
    }

    fn input_buffer_text(agent: &InteractiveAgent) -> String {
        agent
            .input_buffer
            .lock()
            .expect("lock input buffer")
            .clone()
    }

    /// Builds a cd picker over `cwd`, which must contain exactly one
    /// subdirectory so the picker's default selection (index 0) is
    /// unambiguous.
    fn cd_picker_over(cwd: &str) -> SuggestionPicker {
        let picker = SuggestionPicker::for_cd("", cwd, &SessionHistory::default());
        assert_eq!(
            picker.items.len(),
            1,
            "test fixture must contain exactly one subdirectory"
        );
        picker
    }

    #[test]
    fn cd_picker_enter_runs_cd_immediately_in_warp_mode() {
        let cwd = tempdir().expect("create cwd");
        let sub = cwd.path().join("projectx");
        std::fs::create_dir(&sub).expect("create subdir");
        let cwd_str = cwd.path().to_string_lossy().to_string();

        let agent = spawn_test_terminal("cd-warp", &cwd_str);
        assert!(agent.warp_mode, "spawn_terminal defaults to warp mode");
        let mut app = app_with_focused_terminal(agent);
        app.suggestion_picker = Some(cd_picker_over(&cwd_str));

        handle_suggestion_picker_key(&mut app, KeyCode::Enter).expect("handle enter");

        assert!(
            app.suggestion_picker.is_none(),
            "picker must close on Enter"
        );
        let updated = &app.terminal_agents[0];
        assert_eq!(input_buffer_text(updated), "", "input box must be clean");
        assert_eq!(updated.warp_cursor, 0);
        assert!(
            !updated.warp_passthrough,
            "submit must leave warp_passthrough reset, not mid-passthrough"
        );
        assert_eq!(
            PathBuf::from(&updated.working_dir),
            sub.canonicalize().expect("canonicalize subdir"),
            "cwd must reflect the selected directory after a single Enter"
        );
    }

    #[test]
    fn cd_picker_enter_runs_cd_immediately_outside_warp_mode() {
        let cwd = tempdir().expect("create cwd");
        let sub = cwd.path().join("projecty");
        std::fs::create_dir(&sub).expect("create subdir");
        let cwd_str = cwd.path().to_string_lossy().to_string();

        let mut agent = spawn_test_terminal("cd-direct", &cwd_str);
        agent.warp_mode = false;
        let mut app = app_with_focused_terminal(agent);
        app.suggestion_picker = Some(cd_picker_over(&cwd_str));

        handle_suggestion_picker_key(&mut app, KeyCode::Enter).expect("handle enter");

        assert!(app.suggestion_picker.is_none());
        let updated = &app.terminal_agents[0];
        assert_eq!(
            input_buffer_text(updated),
            "",
            "shadow input box must be clean after auto-submit"
        );
        assert_eq!(
            PathBuf::from(&updated.working_dir),
            sub.canonicalize().expect("canonicalize subdir"),
            "cwd must reflect the selected directory after a single Enter"
        );
    }

    #[test]
    fn command_history_picker_enter_only_inserts_without_submitting() {
        let cwd = tempdir().expect("create cwd");
        let cwd_str = cwd.path().to_string_lossy().to_string();
        let agent = spawn_test_terminal("history", &cwd_str);
        let mut app = app_with_focused_terminal(agent);

        app.suggestion_picker = Some(SuggestionPicker {
            input: String::new(),
            mode: PickerMode::CommandHistory,
            items: vec![crate::tui::terminal_history::SuggestionItem {
                text: "echo hi".to_string(),
                label: "echo hi".to_string(),
                count: 1,
            }],
            all_items: vec![],
            selected: 0,
            scroll_offset: 0,
            cd_base_dir: None,
            cd_current_dir: None,
        });

        handle_suggestion_picker_key(&mut app, KeyCode::Enter).expect("handle enter");

        assert!(app.suggestion_picker.is_none(), "picker must close");
        let updated = &app.terminal_agents[0];
        assert_eq!(
            input_buffer_text(updated),
            "echo hi",
            "history completion must only insert, leaving room to edit before running"
        );
        assert_eq!(updated.warp_cursor, "echo hi".len());
        assert_eq!(
            updated.working_dir, cwd_str,
            "non-cd completions must not touch the working directory"
        );
    }

    #[test]
    fn esc_closes_cd_picker_without_running_anything() {
        let cwd = tempdir().expect("create cwd");
        let sub = cwd.path().join("untouched");
        std::fs::create_dir(&sub).expect("create subdir");
        let cwd_str = cwd.path().to_string_lossy().to_string();

        let agent = spawn_test_terminal("cd-esc", &cwd_str);
        let mut app = app_with_focused_terminal(agent);
        app.suggestion_picker = Some(cd_picker_over(&cwd_str));

        handle_suggestion_picker_key(&mut app, KeyCode::Esc).expect("handle esc");

        assert!(app.suggestion_picker.is_none());
        let updated = &app.terminal_agents[0];
        assert_eq!(
            updated.working_dir, cwd_str,
            "Esc must not run the highlighted cd"
        );
        assert_eq!(input_buffer_text(updated), "");
    }
}
