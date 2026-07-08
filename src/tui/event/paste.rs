use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use super::context_transfer::{active_split_session_name, resolve_session};
use super::handle_key;
use super::terminal_warp::sync_terminal_warp_buffer_from_pty;
use crate::tui::app::types::{AgentEntry, App, Focus};

// ── Paste handling (bracketed paste) ─────────────────────────────────

/// Handle pasted text — uses bracketed paste to send text to the PTY without
/// triggering multiple Enter key presses. Preserves newlines for code/YAML/etc.
pub fn handle_paste(app: &mut App, text: &str) {
    match app.focus {
        Focus::Agent => {
            let (vec, idx) = if app.active_split_id.is_some() {
                // Split layouts route paste to whichever panel (session)
                // currently has focus, not the split group itself.
                let Some(name) = active_split_session_name(app) else {
                    return;
                };
                let name = name.to_string();
                resolve_session(app, &name)
            } else {
                match app.selected_agent() {
                    Some(AgentEntry::Interactive(idx)) => ("interactive", *idx),
                    Some(AgentEntry::Terminal(idx)) => ("terminal", *idx),
                    _ => return,
                }
            };
            if idx == usize::MAX {
                return;
            }

            let agent = if vec == "terminal" {
                app.terminal_agents.get_mut(idx)
            } else {
                app.interactive_agents.get_mut(idx)
            };
            if let Some(agent) = agent {
                let bypass = agent.should_bypass_warp_input();
                if agent.warp_mode && !bypass && !agent.warp_passthrough {
                    // Warp prompt editing: insert into input buffer at cursor
                    // (preserves newlines until Enter submits the command).
                    if let Ok(mut buf) = agent.input_buffer.lock() {
                        let pos = agent.warp_cursor.min(buf.len());
                        buf.insert_str(pos, text);
                        agent.warp_cursor = pos + text.len();
                    }
                } else {
                    // Direct to the PTY — wizards, passthrough, and non-warp
                    // sessions alike. Bracketed markers only when the child
                    // program actually enabled bracketed paste mode.
                    let _ = agent.paste_to_pty(text);
                    if agent.warp_mode && agent.warp_passthrough && !bypass {
                        sync_terminal_warp_buffer_from_pty(app, idx, 35);
                    }
                }
            }
        }
        Focus::NewAgentDialog | Focus::PromptTemplateDialog => {
            // Insert pasted text into the SimplePromptDialog sections.
            // Multi-line pastes are collapsed to a placeholder while keeping the real text.
            let field_width = super::prompt_template::prompt_field_width(app);
            if let Some(dialog) = &mut app.simple_prompt_dialog {
                if dialog.enabled_sections.len() > dialog.focused_section {
                    let section_name = dialog.enabled_sections[dialog.focused_section].clone();
                    if text.contains('\n') || text.chars().count() > 200 {
                        // Preserve newlines for collapsed multi-line paste
                        let clean = text.replace('\r', "");
                        dialog.insert_collapsed_paste_at_cursor(&section_name, &clean, field_width);
                    } else {
                        let clean = text.replace('\n', " ").replace('\r', "");
                        dialog.insert_text_at_cursor(&section_name, &clean, field_width);
                    }
                }
            }
            // New-agent dialog has its own prompt input box; route pasted text
            // there too. Newlines are preserved as hard breaks (the renderer
            // wraps with `prompt_visual_line_count` math).
            if let Some(dialog) = &mut app.new_agent_dialog {
                let clean = text.replace('\r', "");
                super::new_agent_dialog::insert_prompt_text(dialog, &clean);
            }
        }
        _ => {
            // For other contexts, simulate typing each char (no newlines)
            let clean = text.replace('\n', " ").replace('\r', "");
            for c in clean.chars() {
                let _ = handle_key(app, KeyCode::Char(c), KeyModifiers::NONE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::models::{SplitGroup, SplitOrientation};
    use crate::tui::agent::InteractiveAgent;
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    /// `cat` is used as a stand-in shell: it never touches the alternate
    /// screen or looks like a sensitive prompt, so pasted text lands in the
    /// warp input buffer instead of going straight to the PTY.
    fn spawn_test_terminal(name: &str) -> InteractiveAgent {
        InteractiveAgent::spawn_terminal(
            "cat",
            "/tmp",
            80,
            24,
            Some(name),
            &[],
            ratatui::style::Color::White,
        )
        .expect("spawn terminal")
    }

    fn app_with_split(session_a: &str, session_b: &str, right_focused: bool) -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.terminal_agents.push(spawn_test_terminal(session_a));
        app.terminal_agents.push(spawn_test_terminal(session_b));
        app.split_groups.push(SplitGroup {
            id: "split-1".to_string(),
            orientation: SplitOrientation::Horizontal,
            session_a: session_a.to_string(),
            session_b: session_b.to_string(),
            created_at: Utc::now(),
        });
        app.active_split_id = Some("split-1".to_string());
        app.split_right_focused = right_focused;
        app.focus = Focus::Agent;
        app
    }

    fn input_buffer_text(agent: &InteractiveAgent) -> String {
        agent
            .input_buffer
            .lock()
            .expect("lock input buffer")
            .clone()
    }

    #[test]
    fn bracketed_paste_routes_to_left_panel_when_left_focused() {
        let mut app = app_with_split("left-term", "right-term", false);

        handle_paste(&mut app, "hello");

        assert_eq!(input_buffer_text(&app.terminal_agents[0]), "hello");
        assert_eq!(input_buffer_text(&app.terminal_agents[1]), "");
    }

    #[test]
    fn bracketed_paste_routes_to_right_panel_when_right_focused() {
        let mut app = app_with_split("left-term", "right-term", true);

        handle_paste(&mut app, "world");

        assert_eq!(input_buffer_text(&app.terminal_agents[0]), "");
        assert_eq!(input_buffer_text(&app.terminal_agents[1]), "world");
    }

    #[test]
    fn bracketed_paste_in_split_is_a_noop_when_session_is_stale() {
        // Regression guard: previously this branch called
        // `resolve_session(app, split_id)`, treating the split's own ID as a
        // session name — it never matched, so paste silently no-opped for
        // every split. Confirm the still-broken/missing-session case stays a
        // clean no-op (not a panic) now that resolution goes through the
        // focused session name.
        let mut app = app_with_split("left-term", "right-term", false);
        app.split_groups[0].session_a = "gone".to_string();

        handle_paste(&mut app, "text");

        assert_eq!(input_buffer_text(&app.terminal_agents[0]), "");
        assert_eq!(input_buffer_text(&app.terminal_agents[1]), "");
    }
}
