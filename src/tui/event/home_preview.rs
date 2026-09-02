use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::app::types::{AgentEntry, App, Focus, LoopLiveFocus, ProjectTab, SidebarLayer};

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

    let has_sidebar_preview = !app.agents.is_empty()
        || !app.projects.is_empty()
        || !app.visible_loops().is_empty()
        || app.rag_info.has_rag_activity();

    match code {
        KeyCode::F(10) => {
            if app.focus == Focus::Agent {
                app.focus = Focus::Preview;
            } else if app.focus == Focus::Preview {
                app.focus = Focus::Home;
            } else {
                app.quit_confirm = true;
            }
        }
        KeyCode::Esc => {
            if app.focus == Focus::Agent {
                app.focus = Focus::Preview;
            } else if app.focus == Focus::Preview {
                app.focus = Focus::Home;
            } else {
                app.quit_confirm = true;
            }
        }
        KeyCode::F(1) => {
            app.show_legend = true;
        }
        KeyCode::Down | KeyCode::Char('j') if has_sidebar_preview => {
            app.dismiss_brain();
            app.focus_sidebar_from_edge(true);
            app.log_scroll = 0;
            app.focus = Focus::Preview;
        }
        KeyCode::Up | KeyCode::Char('k') if has_sidebar_preview => {
            app.dismiss_brain();
            app.focus_sidebar_from_edge(false);
            app.log_scroll = 0;
            app.focus = Focus::Preview;
        }
        KeyCode::Enter if has_sidebar_preview => {
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
    if app.archive_loop_confirm {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let _ = app.archive_selected_loop();
                app.archive_loop_confirm = false;
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.archive_loop_confirm = false;
            }
            _ => {}
        }
        return Ok(());
    }
    if app.permanent_delete_loop_confirm {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let _ = app.permanent_delete_selected_archived_loop();
                app.permanent_delete_loop_confirm = false;
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.permanent_delete_loop_confirm = false;
            }
            _ => {}
        }
        return Ok(());
    }
    if app.loop_reset_confirm {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                app.confirm_reset_selected_loop();
                app.loop_reset_confirm = false;
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.loop_reset_confirm = false;
            }
            _ => {}
        }
        return Ok(());
    }
    if app.loop_autorun_dialog.is_some() {
        use crate::tui::app::dialog::LoopAutorunMode;
        match code {
            KeyCode::Esc => app.close_loop_autorun_dialog(),
            KeyCode::Enter => app.submit_loop_autorun_dialog(),
            // Tab switches between picking a time and typing a raw
            // quota-reset message — the two mutually exclusive submit paths.
            KeyCode::Tab | KeyCode::BackTab => {
                if let Some(dialog) = app.loop_autorun_dialog.as_mut() {
                    dialog.toggle_mode();
                }
            }
            KeyCode::Up
                if app.loop_autorun_dialog.as_ref().map(|d| d.mode)
                    == Some(LoopAutorunMode::Picker) =>
            {
                if let Some(dialog) = app.loop_autorun_dialog.as_mut() {
                    dialog.picker.adjust(1);
                    dialog.error = None;
                }
            }
            KeyCode::Down
                if app.loop_autorun_dialog.as_ref().map(|d| d.mode)
                    == Some(LoopAutorunMode::Picker) =>
            {
                if let Some(dialog) = app.loop_autorun_dialog.as_mut() {
                    dialog.picker.adjust(-1);
                    dialog.error = None;
                }
            }
            KeyCode::Left
                if app.loop_autorun_dialog.as_ref().map(|d| d.mode)
                    == Some(LoopAutorunMode::Picker) =>
            {
                if let Some(dialog) = app.loop_autorun_dialog.as_mut() {
                    dialog.picker.move_field(-1);
                }
            }
            KeyCode::Right
                if app.loop_autorun_dialog.as_ref().map(|d| d.mode)
                    == Some(LoopAutorunMode::Picker) =>
            {
                if let Some(dialog) = app.loop_autorun_dialog.as_mut() {
                    dialog.picker.move_field(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(dialog) = app.loop_autorun_dialog.as_mut() {
                    if dialog.mode == LoopAutorunMode::QuotaMessage {
                        dialog.quota_input.pop();
                    }
                }
            }
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(dialog) = app.loop_autorun_dialog.as_mut() {
                    match dialog.mode {
                        LoopAutorunMode::Picker => {
                            if let Some(digit) = c.to_digit(10) {
                                dialog.picker.type_digit(digit);
                                dialog.error = None;
                            }
                        }
                        LoopAutorunMode::QuotaMessage => {
                            dialog.quota_input.push(c);
                        }
                    }
                }
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

    let on_loop = app.sidebar_layer == SidebarLayer::Automation
        && app.automation_kind == crate::tui::app::AutomationKind::Loop;

    match code {
        // A spec-strip selection intercepts Esc first (back to the graph
        // sub-focus, selection cleared); manual node inspection in the
        // graph intercepts a following Esc to return to auto-follow; only
        // then does Esc fall through to the general "back to Home" below.
        KeyCode::Esc if on_loop && app.loop_live_focus == LoopLiveFocus::SpecStrip => {
            app.loop_live_focus = LoopLiveFocus::Graph;
            app.loop_spec_strip_selected = None;
        }
        KeyCode::Esc if on_loop && !app.loop_graph_follow => {
            app.loop_graph_reset_follow();
        }
        KeyCode::Esc | KeyCode::Char('h') => {
            app.focus = Focus::Home;
        }
        KeyCode::Enter | KeyCode::Char('l') => {
            if app.agents_rag_focused {
                app.activate_playground();
                return Ok(());
            }
            if app.sidebar_layer == SidebarLayer::Knowledge {
                app.enter_project_focus(ProjectTab::Overview);
                app.focus = Focus::Agent;
                return Ok(());
            }
            if on_loop {
                let _ = app.open_loop_editor_dialog();
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
            app.select_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.select_prev();
        }
        // Plain Tab/BackTab hand arrow-key ownership between the graph and
        // the spec marker strip — the strip participates in the panel's
        // existing focus order rather than a bespoke mode. Shift+←/→ is
        // already claimed globally for the sidebar tab strip (see
        // `sidebar_tab_step_applies`), so this uses plain Tab instead.
        KeyCode::Tab | KeyCode::BackTab if on_loop => {
            app.loop_live_toggle_focus();
        }
        KeyCode::Left if on_loop && app.loop_live_focus == LoopLiveFocus::SpecStrip => {
            app.loop_spec_strip_move_selection(false);
        }
        KeyCode::Right if on_loop && app.loop_live_focus == LoopLiveFocus::SpecStrip => {
            app.loop_spec_strip_move_selection(true);
        }
        KeyCode::Left if on_loop => {
            app.loop_graph_move_highlight(false);
        }
        KeyCode::Right if on_loop => {
            app.loop_graph_move_highlight(true);
        }
        KeyCode::Char('[') if on_loop => {
            app.cycle_loop_spec(false);
        }
        KeyCode::Char(']') if on_loop => {
            app.cycle_loop_spec(true);
        }
        KeyCode::Char('e') if !app.agents_rag_focused => {
            if on_loop {
                let _ = app.open_loop_editor_dialog();
            } else if app.sidebar_layer != SidebarLayer::Knowledge {
                app.open_edit_dialog();
            }
        }
        KeyCode::Char('d') if on_loop => {
            // U10: duplicate the highlighted loop node in place.
            let _ = app.duplicate_selected_loop_node();
        }
        // Edit the highlighted node's outgoing pass/fail/always edges
        // (retarget or delete) — a router's route edges stay under 'e'/
        // Enter's RouterRoutes dialog instead.
        KeyCode::Char('w') if on_loop => {
            let _ = app.open_loop_edges_dialog();
        }
        KeyCode::Char('d')
            if !app.agents_rag_focused && app.sidebar_layer != SidebarLayer::Knowledge =>
        {
            let _ = app.toggle_enable();
        }
        // Loop run-time controls (run/pause/continue/reset/autorun),
        // delegated to the daemon's MCP tools — see
        // `app::dialog::loop_control`. Only the action valid for the
        // focused loop's current status actually does anything; the others
        // are simply absent from the footer's hints.
        KeyCode::Char('r') if on_loop => {
            app.run_selected_loop();
        }
        KeyCode::Char('p') if on_loop => {
            app.pause_selected_loop();
        }
        KeyCode::Char('c') if on_loop => {
            app.continue_selected_loop_retry();
        }
        KeyCode::Char('C') if on_loop => {
            app.continue_selected_loop_skip();
        }
        KeyCode::Char('x') if on_loop => {
            app.open_loop_reset_confirm();
        }
        KeyCode::Char('a') if on_loop => {
            app.open_loop_autorun_dialog();
        }
        KeyCode::Char('r')
            if !app.agents_rag_focused && app.sidebar_layer != SidebarLayer::Knowledge =>
        {
            let _ = app.rerun_selected();
        }
        KeyCode::Char('R') if app.sidebar_layer == SidebarLayer::Knowledge => {
            let _ = app.open_project_relation_dialog();
        }
        KeyCode::Char('p') if app.agents_rag_focused => {
            app.toggle_rag_pause();
        }
        KeyCode::Char('n') => {
            if on_loop {
                app.open_new_loop_dialog();
            } else if app.sidebar_layer != SidebarLayer::Knowledge {
                app.open_new_agent_dialog();
            }
        }
        KeyCode::Char('E') if on_loop => {
            app.open_edit_loop_dialog();
        }
        // 'A' toggles the Loops section between the main list and the
        // archive — the archive's only entry point, deliberately a toggle
        // on the existing section rather than a separate sidebar layer, so
        // archived loops stay in the same mental place as active ones.
        KeyCode::Char('A') if on_loop => {
            app.toggle_loop_archive_view();
        }
        // 'R' restores the highlighted archived loop back to the main list.
        KeyCode::Char('R') if on_loop && app.loop_view_archived => {
            let _ = app.restore_selected_archived_loop();
        }
        KeyCode::Char('t') if modifiers.contains(KeyModifiers::CONTROL) => {
            if matches!(
                app.selected_agent(),
                Some(AgentEntry::Interactive(_)) | Some(AgentEntry::Terminal(_))
            ) {
                app.open_context_transfer_modal();
            }
        }
        KeyCode::F(4) => {
            if app.sidebar_layer == SidebarLayer::Knowledge {
                app.delete_project_confirm = true;
            } else if on_loop && app.loop_view_archived {
                // Permanent deletion is reachable only from the archive, on
                // an already-archived loop — never the first press of F4.
                app.permanent_delete_loop_confirm = true;
            } else if on_loop {
                app.archive_loop_confirm = true;
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
            app.focus = Focus::Preview;
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
        KeyCode::Up | KeyCode::Down if !app.playground_results.is_empty() => {
            app.playground_selected = crate::tui::selection::move_index(
                app.playground_selected,
                app.playground_results.len(),
                matches!(code, KeyCode::Down),
            );
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

#[cfg(test)]
mod playground_key_tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::models::{Agent, Cli, Trigger};
    use crate::tui::app::types::App;
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn cron_agent(id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "prompt".to_string(),
            trigger: Some(Trigger::Cron {
                schedule_expr: "0 9 * * *".to_string(),
            }),
            cli: Cli::new("claude"),
            model: None,
            effort: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: Utc::now(),
            log_path: "/tmp/test-playground.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    fn app_with_agents() -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        app
    }

    #[test]
    fn playground_inactive_returns_false() {
        let mut app = app_with_agents();
        app.playground_active = false;
        assert!(!handle_playground_key(
            &mut app,
            KeyCode::Char('a'),
            KeyModifiers::NONE
        ));
    }

    #[test]
    fn playground_active_returns_true_for_any_key() {
        let mut app = app_with_agents();
        app.playground_active = true;
        assert!(handle_playground_key(
            &mut app,
            KeyCode::Char('a'),
            KeyModifiers::NONE
        ));
    }

    #[test]
    fn playground_f10_deactivates() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_selected = 2;
        handle_playground_key(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert!(!app.playground_active);
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn playground_detail_mode_esc_closes_detail() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = true;
        app.playground_scroll = 5;
        handle_playground_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.playground_detail_mode);
        assert_eq!(app.playground_scroll, 0);
        assert!(app.playground_active, "should stay active");
    }

    #[test]
    fn playground_detail_mode_f10_closes_detail() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = true;
        handle_playground_key(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert!(!app.playground_detail_mode);
    }

    #[test]
    fn playground_up_navigates_results() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_selected = 2;
        app.playground_results = (0..4)
            .map(|i| crate::rag::vector_store::SearchResult {
                id: format!("id{i}"),
                file_path: format!("r{i}"),
                content: format!("r{i}"),
                created_at: 0,
                distance: None,
            })
            .collect();
        handle_playground_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.playground_selected, 1);
    }

    #[test]
    fn playground_up_at_zero_stays() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_selected = 0;
        app.playground_results = vec![crate::rag::vector_store::SearchResult {
            id: "id0".into(),
            file_path: "r0".into(),
            content: "r0".into(),
            created_at: 0,
            distance: None,
        }];
        handle_playground_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.playground_selected, 0);
    }

    #[test]
    fn playground_down_navigates_results() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_selected = 0;
        app.playground_results = (0..3)
            .map(|i| crate::rag::vector_store::SearchResult {
                id: format!("id{i}"),
                file_path: format!("r{i}"),
                content: format!("r{i}"),
                created_at: 0,
                distance: None,
            })
            .collect();
        handle_playground_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.playground_selected, 1);
    }

    #[test]
    fn playground_down_at_end_stays() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_results = vec![crate::rag::vector_store::SearchResult {
            id: "id0".into(),
            file_path: "r0".into(),
            content: "r0".into(),
            created_at: 0,
            distance: None,
        }];
        app.playground_selected = 0;
        handle_playground_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.playground_selected, 0);
    }

    #[test]
    fn playground_backspace_removes_char() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_query = "hello".into();
        handle_playground_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.playground_query, "hell");
        assert!(!app.playground_search_pending);
    }

    #[test]
    fn playground_backspace_empty_clears_results() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_query.clear();
        app.playground_results = vec![crate::rag::vector_store::SearchResult {
            id: "id1".into(),
            file_path: "r1".into(),
            content: "r1".into(),
            created_at: 0,
            distance: None,
        }];
        app.playground_selected = 2;
        handle_playground_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert!(app.playground_results.is_empty());
        assert_eq!(app.playground_selected, 0);
    }

    #[test]
    fn playground_char_appends_to_query() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_query.clear();
        handle_playground_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.playground_query, "x");
        assert!(app.playground_search_pending);
    }

    #[test]
    fn playground_ctrl_char_ignored() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_query.clear();
        handle_playground_key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(app.playground_query.is_empty());
    }

    #[test]
    fn playground_detail_shift_up_deactivates() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = true;
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        app.focus = Focus::Agent;
        handle_playground_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(!app.playground_active);
    }

    #[test]
    fn playground_detail_shift_down_deactivates() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = true;
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        app.focus = Focus::Agent;
        handle_playground_key(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert!(!app.playground_active);
    }

    #[test]
    fn playground_non_detail_shift_up_deactivates() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = false;
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        app.focus = Focus::Agent;
        handle_playground_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(!app.playground_active);
    }

    #[test]
    fn playground_detail_scroll_down() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = true;
        app.playground_scroll = 0;
        handle_playground_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.playground_scroll, 3);
    }

    #[test]
    fn playground_detail_scroll_up() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = true;
        app.playground_scroll = 5;
        handle_playground_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.playground_scroll, 2);
    }

    #[test]
    fn playground_detail_scroll_up_saturates() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = true;
        app.playground_scroll = 1;
        handle_playground_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.playground_scroll, 0);
    }

    #[test]
    fn playground_enter_triggers_search_when_query_changed() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_query = "test".into();
        app.playground_last_executed_query = "other".into();
        handle_playground_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.playground_search_pending);
    }

    #[test]
    fn playground_enter_opens_detail_when_results_match() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_results = vec![crate::rag::vector_store::SearchResult {
            id: "id1".into(),
            file_path: "r1".into(),
            content: "r1".into(),
            created_at: 0,
            distance: None,
        }];
        app.playground_query = "r1".into();
        app.playground_last_executed_query = "r1".into();
        handle_playground_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.playground_detail_mode);
    }

    #[test]
    fn playground_ctrl_t_opens_rag_transfer() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_results = vec![crate::rag::vector_store::SearchResult {
            id: "id1".into(),
            file_path: "r1".into(),
            content: "r1".into(),
            created_at: 0,
            distance: None,
        }];
        handle_playground_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(app.rag_transfer_modal.is_some());
    }

    #[test]
    fn playground_detail_ctrl_t_opens_rag_transfer() {
        let mut app = app_with_agents();
        app.playground_active = true;
        app.playground_detail_mode = true;
        app.playground_results = vec![crate::rag::vector_store::SearchResult {
            id: "id1".into(),
            file_path: "r1".into(),
            content: "r1".into(),
            created_at: 0,
            distance: None,
        }];
        handle_playground_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(app.rag_transfer_modal.is_some());
    }
}

#[cfg(test)]
mod home_key_tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::models::{Agent, Cli, Trigger};
    use crate::tui::app::types::App;
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn cron_agent(id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "prompt".to_string(),
            trigger: Some(Trigger::Cron {
                schedule_expr: "0 9 * * *".to_string(),
            }),
            cli: Cli::new("claude"),
            model: None,
            effort: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: Utc::now(),
            log_path: "/tmp/test-home.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    fn app_with_agents() -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        app
    }

    #[test]
    fn home_f10_shows_quit_confirm() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        handle_home_key(&mut app, KeyCode::F(10), KeyModifiers::NONE).unwrap();
        assert!(app.quit_confirm);
        assert!(app.running);
    }

    #[test]
    fn home_quit_confirm_y_exits() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        app.quit_confirm = true;
        handle_home_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).unwrap();
        assert!(!app.running);
    }

    #[test]
    fn home_quit_confirm_enter_exits() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        app.quit_confirm = true;
        handle_home_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(!app.running);
    }

    #[test]
    fn home_quit_confirm_other_key_cancels() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        app.quit_confirm = true;
        handle_home_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).unwrap();
        assert!(!app.quit_confirm);
        assert!(app.running);
    }

    #[test]
    fn home_down_moves_to_preview() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        handle_home_key(&mut app, KeyCode::Down, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn home_up_moves_to_preview() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        handle_home_key(&mut app, KeyCode::Up, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn home_j_moves_to_preview() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        handle_home_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn home_k_moves_to_preview() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        handle_home_key(&mut app, KeyCode::Char('k'), KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn home_enter_moves_to_preview() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        handle_home_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn home_n_opens_new_agent_dialog() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        handle_home_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::NewAgentDialog));
    }

    #[test]
    fn home_f1_shows_legend() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        app.show_legend = false;
        handle_home_key(&mut app, KeyCode::F(1), KeyModifiers::NONE).unwrap();
        assert!(app.show_legend);
    }

    #[test]
    fn home_esc_shows_quit_confirm() {
        let mut app = app_with_agents();
        app.focus = Focus::Home;
        handle_home_key(&mut app, KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(app.quit_confirm);
    }
}

#[cfg(test)]
mod preview_key_tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::models::{Agent, Cli, Trigger};
    use crate::tui::app::types::App;
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn cron_agent(id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "prompt".to_string(),
            trigger: Some(Trigger::Cron {
                schedule_expr: "0 9 * * *".to_string(),
            }),
            cli: Cli::new("claude"),
            model: None,
            effort: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: Utc::now(),
            log_path: "/tmp/test-preview.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    fn app_with_agents() -> App {
        let db = test_db();
        let data_dir = tempdir().expect("create data dir");
        let mut app = App::new(Arc::clone(&db), data_dir.path()).expect("create app");
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        app
    }

    #[test]
    fn preview_esc_goes_home() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        handle_preview_key(&mut app, KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Home));
    }

    #[test]
    fn preview_h_goes_home() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        handle_preview_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Home));
    }

    #[test]
    fn preview_f10_goes_home() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        handle_preview_key(&mut app, KeyCode::F(10), KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Home));
    }

    #[test]
    fn preview_down_selects_next() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.agents = vec![
            AgentEntry::Agent(cron_agent("a1")),
            AgentEntry::Agent(cron_agent("a2")),
        ];
        app.selected = 0;
        handle_preview_key(&mut app, KeyCode::Down, KeyModifiers::NONE).unwrap();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn preview_up_selects_prev() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.agents = vec![
            AgentEntry::Agent(cron_agent("a1")),
            AgentEntry::Agent(cron_agent("a2")),
        ];
        app.selected = 1;
        handle_preview_key(&mut app, KeyCode::Up, KeyModifiers::NONE).unwrap();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn preview_j_selects_next() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.agents = vec![
            AgentEntry::Agent(cron_agent("a1")),
            AgentEntry::Agent(cron_agent("a2")),
        ];
        app.selected = 0;
        handle_preview_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE).unwrap();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn preview_k_selects_prev() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.agents = vec![
            AgentEntry::Agent(cron_agent("a1")),
            AgentEntry::Agent(cron_agent("a2")),
        ];
        app.selected = 1;
        handle_preview_key(&mut app, KeyCode::Char('k'), KeyModifiers::NONE).unwrap();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn preview_enter_focuses_agent() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        handle_preview_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Agent));
    }

    #[test]
    fn preview_l_focuses_agent() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        handle_preview_key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::Agent));
    }

    #[test]
    fn preview_f1_shows_legend() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.show_legend = false;
        handle_preview_key(&mut app, KeyCode::F(1), KeyModifiers::NONE).unwrap();
        assert!(app.show_legend);
    }

    #[test]
    fn preview_n_opens_new_agent_dialog() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        handle_preview_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).unwrap();
        assert!(matches!(app.focus, Focus::NewAgentDialog));
    }

    #[test]
    fn preview_delete_confirm_y_deletes() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.delete_project_confirm = true;
        handle_preview_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).unwrap();
        assert!(!app.delete_project_confirm);
    }

    #[test]
    fn preview_delete_confirm_n_cancels() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.delete_project_confirm = true;
        handle_preview_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).unwrap();
        assert!(!app.delete_project_confirm);
    }

    #[test]
    fn preview_archive_loop_confirm_y_archives() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.archive_loop_confirm = true;
        handle_preview_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).unwrap();
        assert!(!app.archive_loop_confirm);
    }

    #[test]
    fn preview_archive_loop_confirm_n_cancels() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.archive_loop_confirm = true;
        handle_preview_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).unwrap();
        assert!(!app.archive_loop_confirm);
    }

    #[test]
    fn preview_permanent_delete_loop_confirm_y_deletes() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.permanent_delete_loop_confirm = true;
        handle_preview_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).unwrap();
        assert!(!app.permanent_delete_loop_confirm);
    }

    #[test]
    fn preview_permanent_delete_loop_confirm_n_cancels() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.permanent_delete_loop_confirm = true;
        handle_preview_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).unwrap();
        assert!(!app.permanent_delete_loop_confirm);
    }

    #[test]
    fn preview_f4_on_loop_opens_archive_confirm_not_permanent_delete() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.automation_kind = crate::tui::app::AutomationKind::Loop;
        app.loop_view_archived = false;
        handle_preview_key(&mut app, KeyCode::F(4), KeyModifiers::NONE).unwrap();
        assert!(app.archive_loop_confirm);
        assert!(!app.permanent_delete_loop_confirm);
    }

    #[test]
    fn preview_f4_in_archived_view_opens_permanent_delete_confirm() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.automation_kind = crate::tui::app::AutomationKind::Loop;
        app.loop_view_archived = true;
        handle_preview_key(&mut app, KeyCode::F(4), KeyModifiers::NONE).unwrap();
        assert!(app.permanent_delete_loop_confirm);
        assert!(!app.archive_loop_confirm);
    }

    #[test]
    fn preview_shift_a_toggles_loop_archive_view() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.automation_kind = crate::tui::app::AutomationKind::Loop;
        assert!(!app.loop_view_archived);

        handle_preview_key(&mut app, KeyCode::Char('A'), KeyModifiers::NONE).unwrap();
        assert!(app.loop_view_archived);

        handle_preview_key(&mut app, KeyCode::Char('A'), KeyModifiers::NONE).unwrap();
        assert!(!app.loop_view_archived);
    }

    #[test]
    fn preview_d_toggles_enable() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        let _ = handle_preview_key(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
        // The toggle call may fail on test agents, but the key is consumed
    }

    #[test]
    fn preview_r_reruns_selected() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        let _ = handle_preview_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    }

    #[test]
    fn preview_e_opens_edit_dialog() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.agents = vec![AgentEntry::Agent(cron_agent("a1"))];
        app.selected = 0;
        let _ = handle_preview_key(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
    }

    // ── Loop run-time controls ──────────────────────────────────────

    fn loop_with_status(status: crate::domain::loops::LoopStatus) -> crate::domain::loops::Loop {
        crate::domain::loops::Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
            id: "lp1".to_string(),
            name: "Nightly review".to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        }
    }

    fn app_on_loop(status: crate::domain::loops::LoopStatus) -> App {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Automation;
        app.automation_kind = crate::tui::app::AutomationKind::Loop;
        app.loops = vec![loop_with_status(status)];
        app.selected_loop_id = Some("lp1".to_string());
        app
    }

    #[test]
    fn preview_r_on_a_completed_loop_dispatches_run() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Completed);
        handle_preview_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE).unwrap();
        assert!(app.loop_action_pending);
        assert!(app.loop_action_rx.is_some());
    }

    #[test]
    fn preview_r_on_a_running_loop_is_not_bound() {
        // Run is not a valid action for a running loop (decision 3) — 'r'
        // must be a no-op, not a dispatch the daemon then refuses.
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Running);
        handle_preview_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE).unwrap();
        assert!(!app.loop_action_pending);
        assert!(app.loop_action_rx.is_none());
    }

    #[test]
    fn preview_p_on_a_running_loop_dispatches_pause() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Running);
        handle_preview_key(&mut app, KeyCode::Char('p'), KeyModifiers::NONE).unwrap();
        assert!(app.loop_action_pending);
    }

    #[test]
    fn preview_p_on_a_non_running_loop_is_not_bound() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Draft);
        handle_preview_key(&mut app, KeyCode::Char('p'), KeyModifiers::NONE).unwrap();
        assert!(!app.loop_action_pending);
    }

    #[test]
    fn preview_c_and_shift_c_on_a_paused_loop_dispatch_continue() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Paused);
        handle_preview_key(&mut app, KeyCode::Char('c'), KeyModifiers::NONE).unwrap();
        assert!(app.loop_action_pending);

        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Paused);
        handle_preview_key(&mut app, KeyCode::Char('C'), KeyModifiers::SHIFT).unwrap();
        assert!(app.loop_action_pending);
    }

    #[test]
    fn preview_continue_keys_on_a_non_paused_loop_are_not_bound() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Running);
        handle_preview_key(&mut app, KeyCode::Char('c'), KeyModifiers::NONE).unwrap();
        assert!(!app.loop_action_pending);
        handle_preview_key(&mut app, KeyCode::Char('C'), KeyModifiers::SHIFT).unwrap();
        assert!(!app.loop_action_pending);
    }

    #[test]
    fn preview_x_on_a_completed_loop_opens_reset_confirm() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Completed);
        handle_preview_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        assert!(app.loop_reset_confirm);
    }

    #[test]
    fn preview_x_on_a_running_loop_is_not_bound() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Running);
        handle_preview_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        assert!(!app.loop_reset_confirm);
    }

    #[test]
    fn loop_reset_confirm_y_dispatches_reset_and_closes_the_modal() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Failed);
        app.loop_reset_confirm = true;
        handle_preview_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).unwrap();
        assert!(!app.loop_reset_confirm);
        assert!(app.loop_action_pending);
    }

    #[test]
    fn loop_reset_confirm_n_cancels_without_dispatching() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Failed);
        app.loop_reset_confirm = true;
        handle_preview_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).unwrap();
        assert!(!app.loop_reset_confirm);
        assert!(!app.loop_action_pending);
    }

    #[test]
    fn preview_a_opens_autorun_dialog_regardless_of_status() {
        for status in [
            crate::domain::loops::LoopStatus::Draft,
            crate::domain::loops::LoopStatus::Running,
            crate::domain::loops::LoopStatus::Paused,
            crate::domain::loops::LoopStatus::Completed,
            crate::domain::loops::LoopStatus::Failed,
        ] {
            let mut app = app_on_loop(status);
            handle_preview_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE).unwrap();
            assert!(app.loop_autorun_dialog.is_some(), "status {status:?}");
        }
    }

    #[test]
    fn loop_autorun_dialog_quota_message_typing_backspace_and_submit() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Failed);
        handle_preview_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE).unwrap();
        // Tab switches into the free-text quota-message mode (the picker is
        // the default mode on open).
        handle_preview_key(&mut app, KeyCode::Tab, KeyModifiers::NONE).unwrap();

        handle_preview_key(&mut app, KeyCode::Char('1'), KeyModifiers::NONE).unwrap();
        handle_preview_key(&mut app, KeyCode::Char('2'), KeyModifiers::NONE).unwrap();
        handle_preview_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        assert_eq!(app.loop_autorun_dialog.as_ref().unwrap().quota_input, "1");

        handle_preview_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(app.loop_autorun_dialog.is_none());
        assert!(app.loop_action_pending);
    }

    #[test]
    fn loop_autorun_dialog_picker_mode_default_seed_is_rejected_on_submit() {
        // The picker seeds to "now" absent a pending autorun (see
        // `LoopAutorunDialog::new`), which is already past by the time
        // submit runs a moment later — the dialog must stay open with an
        // inline error rather than dispatch (requirement 7).
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Failed);
        handle_preview_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE).unwrap();
        handle_preview_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(app.loop_autorun_dialog.is_some());
        assert!(!app.loop_action_pending);
        assert!(app.loop_autorun_dialog.as_ref().unwrap().error.is_some());
    }

    #[test]
    fn loop_autorun_dialog_picker_mode_future_time_submits() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Failed);
        handle_preview_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE).unwrap();
        // Field 0 (year) is focused on open; bump it a year forward so the
        // picked time is unambiguously in the future.
        handle_preview_key(&mut app, KeyCode::Up, KeyModifiers::NONE).unwrap();
        handle_preview_key(&mut app, KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(app.loop_autorun_dialog.is_none());
        assert!(app.loop_action_pending);
    }

    #[test]
    fn loop_autorun_dialog_esc_closes_without_dispatching() {
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Failed);
        handle_preview_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE).unwrap();
        handle_preview_key(&mut app, KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(app.loop_autorun_dialog.is_none());
        assert!(!app.loop_action_pending);
    }

    #[test]
    fn existing_navigation_keys_are_unaffected_by_the_new_bindings() {
        // Arrows/Tab must keep meaning exactly what they meant before —
        // decision 8 of the loop controls spec.
        let mut app = app_on_loop(crate::domain::loops::LoopStatus::Running);
        app.loop_live_state = Some(crate::tui::app::loop_live_state::LoopLiveState {
            loop_id: "lp1".to_string(),
            loop_name: "Nightly review".to_string(),
            loop_status: crate::domain::loops::LoopStatus::Running,
            workdir: "/tmp".to_string(),
            trigger_type: "manual".to_string(),
            schedule_expr: None,
            watch_path: None,
            autorun_at: None,
            spec_queue: Vec::new(),
            done_count: 0,
            total_count: 0,
            current_spec_id: None,
            effective_nodes: vec![crate::domain::loops::LoopNode {
                id: "n1".to_string(),
                spec_id: None,
                loop_id: Some("lp1".to_string()),
                name: "Implement".to_string(),
                kind: crate::domain::loops::LoopNodeKind::Agent,
                config: serde_json::json!({}),
                position: 0,
                created_at: Utc::now(),
            }],
            effective_edges: Vec::new(),
            ensembles: Vec::new(),
            router_taken_routes: std::collections::HashMap::new(),
            current_node_id: None,
            current_node_status: None,
            current_node_started_at: None,
            current_node_iteration: None,
            current_node_output_tail: None,
        });

        let before_follow = app.loop_graph_follow;
        handle_preview_key(&mut app, KeyCode::Right, KeyModifiers::NONE).unwrap();
        // Moving the graph highlight right drops auto-follow, same as always.
        assert_ne!(before_follow, app.loop_graph_follow);

        let before_focus = app.loop_live_focus;
        handle_preview_key(&mut app, KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_ne!(before_focus, app.loop_live_focus);
    }

    fn spawn_cat_agent(name: &str) -> crate::tui::agent::InteractiveAgent {
        crate::tui::agent::InteractiveAgent::spawn(
            Cli::new("cat"),
            ".",
            80,
            24,
            None,
            None,
            ratatui::style::Color::Reset,
            Some(name),
            &[],
            None,
            None,
            None,
        )
        .expect("spawn cat as a stand-in interactive child")
    }

    #[test]
    fn preview_ctrl_t_opens_context_transfer_for_interactive_selection() {
        let mut app = app_with_agents();
        app.interactive_agents = vec![spawn_cat_agent("interactive-1")];
        app.agents = vec![AgentEntry::Interactive(0)];
        app.selected = 0;
        app.focus = Focus::Preview;

        handle_preview_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL).unwrap();

        assert!(matches!(app.focus, Focus::ContextTransfer));
        app.interactive_agents[0].kill();
    }

    #[test]
    fn preview_ctrl_t_does_nothing_for_non_session_selection() {
        let mut app = app_with_agents();
        app.focus = Focus::Preview;

        handle_preview_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL).unwrap();

        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn preview_ctrl_t_opens_rag_transfer_not_context_transfer_while_playground_active() {
        let mut app = app_with_agents();
        app.interactive_agents = vec![spawn_cat_agent("interactive-2")];
        app.agents = vec![AgentEntry::Interactive(0)];
        app.selected = 0;
        app.focus = Focus::Preview;
        app.playground_active = true;

        handle_preview_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL).unwrap();

        assert!(!matches!(app.focus, Focus::ContextTransfer));
        app.interactive_agents[0].kill();
    }
}
