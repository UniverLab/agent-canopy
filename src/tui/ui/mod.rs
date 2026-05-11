//! UI rendering — sidebar with agent cards, log panel, header, footer, and dialogs.

mod dialogs;
mod footer;
mod header;
mod panel;
mod sidebar;
mod system_dashboard;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::Frame;

use super::app::types::App;

// ── Shared palette ──────────────────────────────────────────────

pub(crate) const ACCENT: Color = Color::Rgb(76, 175, 80);
pub(crate) const DIM: Color = Color::Rgb(150, 150, 170);
pub(crate) const ERROR_COLOR: Color = Color::Rgb(229, 57, 53);
pub(crate) const BG_SELECTED: Color = Color::Rgb(20, 40, 20);
pub(crate) const INTERACTIVE_COLOR: Color = Color::Rgb(102, 187, 106);
pub(crate) const STATUS_DISABLED: Color = Color::Rgb(120, 120, 120);
pub(crate) const STATUS_RUNNING: Color = Color::Rgb(76, 175, 80);
pub(crate) const STATUS_OK: Color = Color::Rgb(66, 165, 245);
pub(crate) const STATUS_FAIL: Color = Color::Rgb(229, 57, 53);
pub(crate) const STATUS_WAIT_ON: Color = Color::Rgb(255, 255, 0);
pub(crate) const STATUS_WAIT_OFF: Color = Color::Rgb(30, 30, 30);

// ── Main draw entry point ───────────────────────────────────────

pub fn draw(frame: &mut Frame, app: &mut App) {
    let full = frame.area();
    frame.render_widget(
        ratatui::widgets::Paragraph::new("").style(Style::default().bg(Color::Rgb(18, 18, 18))),
        full,
    );

    let [header_area, body, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let body_area = if app.sidebar_visible {
        let [sidebar, content] =
            Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).areas(body);
        header::draw_header(frame, header_area, app);
        sidebar::draw_sidebar(frame, sidebar, app);
        content
    } else {
        header::draw_header(frame, header_area, app);
        body
    };

    let sync_state = app.sync_panel_state();
    let sync_width = app.sync_panel_layout_width(body_area.width, sync_state.is_some());
    let (panel_area, sync_area) = if let Some(sync_state) = sync_state.as_ref() {
        if sync_width > 0 {
            let [panel, sync] = Layout::horizontal([
                Constraint::Min(body_area.width.saturating_sub(sync_width)),
                Constraint::Length(sync_width),
            ])
            .areas(body_area);
            panel::draw_sync_panel(frame, sync, sync_state, app.sync_scroll_offset);
            app.last_sync_area = Some(sync);
            (panel, Some(sync))
        } else {
            app.last_sync_area = None;
            (body_area, None)
        }
    } else {
        app.last_sync_area = None;
        (body_area, None)
    };

    // Split view: render two panels side-by-side (or stacked) when a split is active
    if let Some(ref split_id) = app.active_split_id.clone() {
        if let Some(group) = app.split_groups.iter().find(|g| g.id == *split_id) {
            let session_a = group.session_a.clone();
            let session_b = group.session_b.clone();
            let orientation = group.orientation;
            let areas = match orientation {
                crate::domain::models::SplitOrientation::Horizontal => {
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .areas(panel_area)
                }
                crate::domain::models::SplitOrientation::Vertical => {
                    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .areas(panel_area)
                }
            };
            let [area_a, area_b]: [Rect; 2] = areas;
            panel::draw_split_panel(frame, area_a, app, &session_a, !app.split_right_focused);
            panel::draw_split_panel(frame, area_b, app, &session_b, app.split_right_focused);
        } else {
            // Group no longer exists — clear stale reference
            app.active_split_id = None;
            panel::draw_log_panel(frame, panel_area, app);
        }
    } else {
        panel::draw_log_panel(frame, panel_area, app);
    }

    footer::draw_footer(frame, footer_area, app);

    if app.new_agent_dialog.is_some() {
        dialogs::draw_new_agent_dialog(frame, app);
    }

    if app.launchpad_dialog.is_some() {
        dialogs::draw_launchpad_dialog(frame, app);
    }

    if app.quit_confirm {
        dialogs::draw_quit_confirm(frame);
    }

    if app.show_legend {
        dialogs::draw_legend(frame, app);
    }

    if app.context_transfer_modal.is_some() {
        dialogs::draw_context_transfer_modal(frame, app);
    }

    if app.rag_transfer_modal.is_some() {
        dialogs::draw_rag_transfer_modal(frame, app);
    }

    if app.simple_prompt_dialog.is_some() {
        dialogs::draw_simple_prompt_dialog(frame, app);
    }

    if app.split_picker_open {
        dialogs::draw_split_picker(frame, app);
    }

    if app.suggestion_picker.is_some() {
        dialogs::draw_suggestion_picker(frame, app, panel_area);
    }

    // Terminal search bar overlay (Ctrl+F)
    if let Some(search) = &app.terminal_search {
        let w = panel_area.width.min(50);
        let x = panel_area.x + panel_area.width.saturating_sub(w + 1);
        let y = panel_area.y;
        let area = Rect::new(x, y, w, 1);
        let match_info = if search.match_rows.is_empty() {
            if search.query.is_empty() {
                String::new()
            } else {
                " (no matches)".to_string()
            }
        } else {
            format!(" {}/{}", search.current_match + 1, search.match_rows.len())
        };
        let text = format!(" 🔍 {}{} ", search.query, match_info);
        let style = ratatui::style::Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(255, 235, 59));
        frame.render_widget(ratatui::widgets::Paragraph::new(text).style(style), area);
    }

    // Top-level overlays rendered last so they appear above all content
    if app.show_copied {
        let full = frame.area();
        let msg = " \u{2592} COPIED \u{2592} "; // ▒ COPIED ▒
        let w = msg.chars().count() as u16; // display width (char count, not bytes)
        if full.width > w + 2 {
            let x = full.x + full.width - w - 1;
            let y = full.y + 1; // just below header
            let area = ratatui::layout::Rect::new(x, y, w, 1);
            let widget = ratatui::widgets::Paragraph::new(msg)
                .style(ratatui::style::Style::default().fg(ACCENT).bg(Color::Black));
            frame.render_widget(widget, area);
        }
    }

    let _ = sync_area;
}

// ── Shared helpers ──────────────────────────────────────────────

/// Create a centered rect of given percentage width and fixed height.
pub(crate) fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let [_, center, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(area);

    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(center);

    center
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max > 1 {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    } else {
        String::new()
    }
}

/// Extract the last two path segments, e.g. `/a/b/c/d` → `c/d`.
pub(crate) fn last_two_segments(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return "/".to_string();
    }
    if parts.len() <= 2 {
        return trimmed.to_string();
    }
    format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
}
