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
                if agent.warp_mode && (agent.should_bypass_warp_input() || agent.warp_passthrough) {
                    let _ = agent.write_to_pty(text.as_bytes());
                    if !agent.should_bypass_warp_input() {
                        sync_terminal_warp_buffer_from_pty(app, idx, 35);
                    }
                } else if agent.warp_mode {
                    // Warp mode: insert into input buffer at cursor (preserves newlines)
                    if let Ok(mut buf) = agent.input_buffer.lock() {
                        let pos = agent.warp_cursor.min(buf.len());
                        buf.insert_str(pos, text);
                        agent.warp_cursor = pos + text.len();
                    }
                } else {
                    // Non-warp: use bracketed paste to preserve newlines without triggering Enter
                    let bracketed = format!("\x1b[200~{}\x1b[201~", text);
                    let _ = agent.write_to_pty(bracketed.as_bytes());
                }
            }
        }
        Focus::NewAgentDialog | Focus::PromptTemplateDialog => {
            // Insert pasted text into the SimplePromptDialog sections.
            // Multi-line pastes are collapsed to a placeholder while keeping the real text.
            if let Some(dialog) = &mut app.simple_prompt_dialog {
                if dialog.enabled_sections.len() > dialog.focused_section {
                    let section_name = dialog.enabled_sections[dialog.focused_section].clone();
                    // Must match render calculation in dialogs.rs
                    let field_width = ((app.term_width as usize * 65 / 100).max(40))
                        .saturating_sub(4)
                        .max(10);
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
