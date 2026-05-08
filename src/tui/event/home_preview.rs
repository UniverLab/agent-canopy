use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::app::types::{AgentEntry, App, Focus, ProjectsPanelFocus, SidebarMode};

// ── Home: screensaver — arrows enter Preview ────────────────────────

pub fn handle_home_key(app: &mut App, code: KeyCode, _modifiers: KeyModifiers) -> Result<()> {
    // Quit-confirmation overlay intercepts all keys
    if app.quit_confirm {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => app.running = false,
            _ => app.quit_confirm = false,
        }
        return Ok(());
    }

    let has_project_preview = app.sidebar_mode == SidebarMode::Projects
        && (!app.projects.is_empty()
            || !app.global_rag_queue.is_empty()
            || app.rag_info.total_chunks > 0);

    match code {
        KeyCode::F(10) if !app.agents.is_empty() => {
            app.dismiss_brain();
            app.log_scroll = 0;
            app.focus = Focus::Preview;
        }
        KeyCode::Esc => {
            app.quit_confirm = true;
        }
        KeyCode::F(1) => {
            app.show_legend = true;
        }
        KeyCode::Down | KeyCode::Char('j') if !app.agents.is_empty() || has_project_preview => {
            app.dismiss_brain();
            if app.sidebar_mode == SidebarMode::Agents {
                // Arrow-down from Home: go to the first agent (background, at top of sidebar).
                app.selected = 0;
                app.agents_rag_focused = false;
            } else {
                // Projects mode: arrow-down goes to RagInfo if chunks exist, else first project.
                if app.rag_info.total_chunks > 0 {
                    app.projects_panel_focus = ProjectsPanelFocus::RagInfo;
                } else {
                    app.select_next();
                }
            }
            app.log_scroll = 0;
            app.focus = Focus::Preview;
        }
        KeyCode::Up | KeyCode::Char('k') if !app.agents.is_empty() || has_project_preview => {
            app.dismiss_brain();
            if app.sidebar_mode == SidebarMode::Agents {
                app.selected = app.agents.len().saturating_sub(1);
                app.agents_rag_focused = false;
            } else {
                app.select_prev();
            }
            app.log_scroll = 0;
            app.focus = Focus::Preview;
        }
        KeyCode::Enter if !app.agents.is_empty() || has_project_preview => {
            app.dismiss_brain();
            app.log_scroll = 0;
            app.focus = Focus::Preview;
        }
        KeyCode::Char('n') => app.open_new_agent_dialog(),
        _ => {}
    }
    Ok(())
}

// ── Preview: navigate agents, Enter → Focus ─────────────────────────

pub fn handle_preview_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    // If playground is active, handle playground-specific inputs
    if app.playground_active {
        if app.playground_detail_mode {
            match code {
                KeyCode::Esc => {
                    app.playground_detail_mode = false;
                    app.playground_scroll = 0;
                }
                KeyCode::F(10) => {
                    app.playground_detail_mode = false;
                    app.playground_scroll = 0;
                }
                KeyCode::Up if modifiers.contains(KeyModifiers::SHIFT) => {
                    app.deactivate_playground();
                    app.select_prev();
                }
                KeyCode::Down if modifiers.contains(KeyModifiers::SHIFT) => {
                    app.deactivate_playground();
                    app.select_next();
                }
                KeyCode::Up => {
                    app.playground_scroll = app.playground_scroll.saturating_sub(3);
                }
                KeyCode::Down => {
                    app.playground_scroll = app.playground_scroll.saturating_add(3);
                }
                KeyCode::Char('t') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.open_rag_transfer_modal();
                }
                _ => {}
            }
        } else {
            match code {
                KeyCode::F(10) => {
                    app.deactivate_playground();
                }
                KeyCode::Up if app.playground_selected > 0 => {
                    app.playground_selected -= 1;
                }
                KeyCode::Down if app.playground_selected + 1 < app.playground_results.len() => {
                    app.playground_selected += 1;
                }
                KeyCode::Enter | KeyCode::Char('l') if !app.playground_results.is_empty() => {
                    app.playground_detail_mode = true;
                    app.playground_scroll = 0;
                }
                KeyCode::Backspace => {
                    app.playground_query.pop();
                    app.playground_last_search = std::time::Instant::now();
                }
                KeyCode::Char('t') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.open_rag_transfer_modal();
                }
                KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    app.playground_query.push(c);
                    app.playground_last_search = std::time::Instant::now();
                }
                _ => {}
            }
        }
        return Ok(());
    }

    match code {
        KeyCode::Esc | KeyCode::Char('h') => {
            app.focus = Focus::Home;
        }
        KeyCode::Enter | KeyCode::Char('l') => {
            if app.sidebar_mode == SidebarMode::Projects {
                match app.projects_panel_focus {
                    ProjectsPanelFocus::RagInfo => app.activate_playground(),
                    ProjectsPanelFocus::Projects => {}
                }
                return Ok(());
            }
            // Agents mode: Enter on focused RagInfo → open playground.
            if app.agents_rag_focused {
                app.activate_playground();
                return Ok(());
            }
            // For Group entries: Enter activates the split and enters focus
            if let Some(AgentEntry::Group(idx)) = app.selected_agent() {
                let idx = *idx;
                if let Some(group) = app.split_groups.get(idx) {
                    let id = group.id.clone();
                    app.active_split_id = Some(id);
                    app.split_right_focused = false;
                }
                app.focus = Focus::Agent;
                return Ok(());
            }
            app.log_scroll = 0;
            app.focus = Focus::Agent;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.sidebar_mode == SidebarMode::Projects {
                // In Projects mode: Down cycles Projects → RagInfo
                if app.projects_panel_focus == ProjectsPanelFocus::Projects
                    && app.rag_info.total_chunks > 0
                {
                    app.projects_panel_focus = ProjectsPanelFocus::RagInfo;
                } else {
                    app.projects_panel_focus = ProjectsPanelFocus::Projects;
                    app.select_next();
                }
            } else {
                // Agents mode: Down navigates agents; if at end and RAG exists, go to RagInfo.
                // If already on RagInfo, scroll the per-file list; at end, wrap back to first agent.
                if app.agents_rag_focused {
                    let max_scroll = app.rag_file_status.len().saturating_sub(1);
                    if app.rag_report_scroll < max_scroll {
                        app.rag_report_scroll += 1;
                    } else {
                        app.agents_rag_focused = false;
                        app.rag_report_scroll = 0;
                        app.selected = 0;
                    }
                } else {
                    let at_last = app.selected + 1 >= app.agents.len();
                    if at_last && app.rag_info.total_chunks > 0 {
                        app.agents_rag_focused = true;
                    } else {
                        app.select_next();
                    }
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.sidebar_mode == SidebarMode::Projects {
                // In Projects mode: Up cycles RagInfo → Projects
                if app.projects_panel_focus == ProjectsPanelFocus::RagInfo {
                    app.projects_panel_focus = ProjectsPanelFocus::Projects;
                } else {
                    app.select_prev();
                }
            } else {
                // Agents mode: Up scrolls in ragInfo; at top, exits back to last agent
                if app.agents_rag_focused {
                    if app.rag_report_scroll > 0 {
                        app.rag_report_scroll -= 1;
                    } else {
                        app.agents_rag_focused = false;
                    }
                } else {
                    app.select_prev();
                }
            }
        }
        KeyCode::Char('e') if !app.agents_rag_focused => {
            app.open_edit_dialog();
        }
        KeyCode::Char('d') if !app.agents_rag_focused => {
            let _ = app.toggle_enable();
        }
        KeyCode::Char('r') if !app.agents_rag_focused => {
            let _ = app.rerun_selected();
        }
        KeyCode::Char('p')
            if app.sidebar_mode == SidebarMode::Projects
                && app.projects_panel_focus == ProjectsPanelFocus::RagInfo =>
        {
            app.toggle_rag_pause();
        }
        KeyCode::Char('n') => app.open_new_agent_dialog(),
        KeyCode::F(4) => {
            if app.sidebar_mode == SidebarMode::Projects
                && app.projects_panel_focus == ProjectsPanelFocus::Projects
            {
                let _ = app.delete_selected_project();
            } else if !app.agents_rag_focused {
                let _ = app.delete_selected();
            }
        }
        KeyCode::F(10) => {
            app.focus = Focus::Home;
        }
        KeyCode::F(1) => {
            app.show_legend = true;
        }
        _ => {}
    }
    Ok(())
}

// ── Focus: PTY interaction or log scroll ────────────────────────────
