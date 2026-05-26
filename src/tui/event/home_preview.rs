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
            || !app.visible_workflows().is_empty()
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
                app.focus_projects_panel_from_edge(true);
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
                app.focus_projects_panel_from_edge(false);
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
    // Modal delete confirm intercepts all keys
    if app.delete_project_confirm {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let _ = app.delete_selected_project();
                app.delete_project_confirm = false;
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.delete_project_confirm = false;
            }
            _ => {}
        }
        return Ok(());
    }
    if app.delete_workflow_confirm {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let _ = app.delete_selected_workflow();
                app.delete_workflow_confirm = false;
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.delete_workflow_confirm = false;
            }
            _ => {}
        }
        return Ok(());
    }
    if handle_playground_key(app, code, modifiers) {
        return Ok(());
    }

    if handle_project_relation_dialog_key(app, code) {
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
                    ProjectsPanelFocus::Projects => {
                        let _ = app.open_project_relation_dialog();
                    }
                    ProjectsPanelFocus::Workflows => {
                        let _ = app.open_workflow_editor_dialog();
                    }
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
        KeyCode::Tab if app.sidebar_mode == SidebarMode::Projects => {
            app.cycle_projects_panel_focus(true);
        }
        KeyCode::BackTab if app.sidebar_mode == SidebarMode::Projects => {
            app.cycle_projects_panel_focus(false);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.sidebar_mode == SidebarMode::Projects {
                app.select_next();
            } else {
                // Agents mode: Down navigates agents; if at end and RAG exists, go to RagInfo.
                if app.agents_rag_focused {
                    app.agents_rag_focused = false;
                    if !app.agents.is_empty() {
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
                app.select_prev();
            } else {
                if app.agents_rag_focused {
                    app.agents_rag_focused = false;
                    if !app.agents.is_empty() {
                        app.selected = app.agents.len() - 1;
                    }
                } else {
                    app.select_prev();
                }
            }
        }
        KeyCode::Left
            if app.sidebar_mode == SidebarMode::Projects
                && app.projects_panel_focus == ProjectsPanelFocus::Workflows =>
        {
            app.cycle_workflow_node(false);
        }
        KeyCode::Right
            if app.sidebar_mode == SidebarMode::Projects
                && app.projects_panel_focus == ProjectsPanelFocus::Workflows =>
        {
            app.cycle_workflow_node(true);
        }
        KeyCode::Char('[')
            if app.sidebar_mode == SidebarMode::Projects
                && app.projects_panel_focus == ProjectsPanelFocus::Workflows =>
        {
            app.cycle_workflow_spec(false);
        }
        KeyCode::Char(']')
            if app.sidebar_mode == SidebarMode::Projects
                && app.projects_panel_focus == ProjectsPanelFocus::Workflows =>
        {
            app.cycle_workflow_spec(true);
        }
        KeyCode::Char('e') if !app.agents_rag_focused => {
            if app.sidebar_mode == SidebarMode::Projects
                && app.projects_panel_focus == ProjectsPanelFocus::Workflows
            {
                let _ = app.open_workflow_editor_dialog();
            } else {
                app.open_edit_dialog();
            }
        }
        KeyCode::Char('d') if !app.agents_rag_focused => {
            let _ = app.toggle_enable();
        }
        KeyCode::Char('r') if !app.agents_rag_focused => {
            let _ = app.rerun_selected();
        }
        KeyCode::Char('R') if app.sidebar_mode == SidebarMode::Projects => {
            let _ = app.open_project_relation_dialog();
        }
        KeyCode::Char('p')
            if app.sidebar_mode == SidebarMode::Projects
                && app.projects_panel_focus == ProjectsPanelFocus::RagInfo =>
        {
            app.toggle_rag_pause();
        }
        KeyCode::Char('n') => app.open_new_agent_dialog(),
        KeyCode::F(4) => {
            if app.sidebar_mode == SidebarMode::Projects {
                match app.projects_panel_focus {
                    ProjectsPanelFocus::Projects => {
                        app.delete_project_confirm = true;
                    }
                    ProjectsPanelFocus::Workflows => {
                        app.delete_workflow_confirm = true;
                    }
                    _ => {}
                }
            } else if app.sidebar_mode != SidebarMode::Projects && !app.agents_rag_focused {
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

pub(super) fn handle_playground_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if !app.playground_active {
        return false;
    }

    if app.playground_detail_mode {
        match code {
            KeyCode::Esc | KeyCode::F(10) => {
                app.playground_detail_mode = false;
                app.playground_scroll = 0;
            }
            KeyCode::Up if modifiers.contains(KeyModifiers::SHIFT) => {
                app.deactivate_playground();
                if app.focus == Focus::Agent {
                    app.prev_interactive();
                } else {
                    app.select_prev();
                }
            }
            KeyCode::Down if modifiers.contains(KeyModifiers::SHIFT) => {
                app.deactivate_playground();
                if app.focus == Focus::Agent {
                    app.next_interactive();
                } else {
                    app.select_next();
                }
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
        return true;
    }

    match code {
        KeyCode::F(10) => {
            app.deactivate_playground();
            if app.focus != Focus::Agent {
                app.focus = Focus::Preview;
            }
        }
        KeyCode::Up if modifiers.contains(KeyModifiers::SHIFT) => {
            app.deactivate_playground();
            if app.focus == Focus::Agent {
                app.prev_interactive();
            } else {
                app.select_prev();
            }
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::SHIFT) => {
            app.deactivate_playground();
            if app.focus == Focus::Agent {
                app.next_interactive();
            } else {
                app.select_next();
            }
        }
        KeyCode::Up if app.playground_selected > 0 => {
            app.playground_selected -= 1;
        }
        KeyCode::Down if app.playground_selected + 1 < app.playground_results.len() => {
            app.playground_selected += 1;
        }
        KeyCode::Enter | KeyCode::Char('l') => {
            if app.playground_last_executed_query != app.playground_query.trim() {
                app.playground_search_pending = true;
                app.playground_last_search =
                    std::time::Instant::now() - std::time::Duration::from_secs(1);
            } else if !app.playground_results.is_empty() {
                app.playground_detail_mode = true;
                app.playground_scroll = 0;
            }
        }
        KeyCode::Backspace => {
            app.playground_query.pop();
            if app.playground_query.is_empty() {
                app.playground_results.clear();
                app.playground_selected = 0;
                app.playground_last_executed_query.clear();
            }
            // Deleting does not trigger auto-search.
            app.playground_search_pending = false;
            app.playground_last_search = std::time::Instant::now();
        }
        KeyCode::Char('t') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_rag_transfer_modal();
        }
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            app.playground_query.push(c);
            app.playground_last_search = std::time::Instant::now();
            app.playground_search_pending = true;
        }
        _ => {}
    }

    true
}

// ── Project Relation Dialog ──────────────────────────────────────

fn handle_project_relation_dialog_key(app: &mut App, code: KeyCode) -> bool {
    if !matches!(app.focus, Focus::ProjectRelationDialog) {
        return false;
    }
    let Some(dialog) = app.project_relation_dialog.as_mut() else {
        return false;
    };

    match code {
        KeyCode::Esc => {
            app.close_project_relation_dialog();
        }
        KeyCode::Enter => {
            let _ = app.confirm_project_relation();
        }
        KeyCode::Up | KeyCode::Char('k') => dialog.move_up(),
        KeyCode::Down | KeyCode::Char('j') => dialog.move_down(),
        KeyCode::Left | KeyCode::Char('h') => dialog.cycle_relation(false),
        KeyCode::Right | KeyCode::Char('l') => dialog.cycle_relation(true),
        KeyCode::Backspace => {
            dialog.filter_buffer.pop();
            dialog.rebuild_filtered();
        }
        KeyCode::Char(c) if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' => {
            dialog.filter_buffer.push(c);
            dialog.rebuild_filtered();
        }
        _ => return true,
    }
    true
}

// ── Focus: PTY interaction or log scroll ────────────────────────────
