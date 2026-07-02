use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use super::context_transfer::resolve_session;
use super::handle_key;
use super::terminal_warp::sync_terminal_warp_buffer_from_pty;
use crate::tui::app::types::{AgentEntry, App, Focus};

// ── Paste handling (bracketed paste) ─────────────────────────────────

/// Handle pasted text — uses bracketed paste to send text to the PTY without
/// triggering multiple Enter key presses. Preserves newlines for code/YAML/etc.
pub fn handle_paste(app: &mut App, text: &str) {
    match app.focus {
        Focus::Agent => {
            let (vec, idx) = if let Some(split_id) = &app.active_split_id {
                let id = split_id.clone();
                resolve_session(app, &id)
            } else {
                match app.selected_agent() {
                    Some(AgentEntry::Interactive(idx)) => ("interactive", *idx),
                    Some(AgentEntry::Terminal(idx)) => ("terminal", *idx),
                    _ => return,
                }
            };

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
