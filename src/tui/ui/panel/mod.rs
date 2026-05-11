//! Right panel rendering — PTY output, brain automaton, banner, background_agent/watcher details, log.

use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::{
    truncate_str, truncate_str_keep_tail, ACCENT, DIM, INTERACTIVE_COLOR, STATUS_DISABLED,
    STATUS_FAIL, STATUS_OK, STATUS_RUNNING,
};
use crate::tui::agent::ScreenSnapshot;
use crate::tui::app::types::{AgentEntry, App, Focus, ProjectsPanelFocus};
use crate::tui::app::SidebarMode;

pub mod details;
pub mod home;
pub mod log_fallback;
pub mod sync;
pub mod vt100;
pub mod warp;

pub use details::{draw_agent_details, draw_group_details};
pub(crate) use home::draw_brians_brain;
pub use log_fallback::draw_log_text;
pub(crate) use sync::draw_sync_panel;
use vt100::render_vt_screen;
#[allow(unused_imports)]
pub use warp::compact_cwd;
pub use warp::{draw_warp_input_box, render_command_chips};

use home::draw_canopy_banner_animation;
use vt100::{render_indicators, render_vt_screen_with_mask};

fn render_panel_block<'a>(
    frame: &mut Frame,
    area: Rect,
    border_color: Color,
    title: Option<Span<'a>>,
) -> Rect {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    if let Some(title) = title {
        block = block.title(title);
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn render_wrapped_paragraph<'a>(frame: &mut Frame, area: Rect, lines: Vec<Line<'a>>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn set_cursor_from_snapshot(frame: &mut Frame, area: Rect, snap: &ScreenSnapshot) {
    if snap.scrolled || area.width == 0 || area.height == 0 {
        return;
    }

    let cx = area.x + snap.cursor_col.min(area.width.saturating_sub(1));
    let cy = area.y + snap.cursor_row.min(area.height.saturating_sub(1));
    frame.set_cursor_position((cx, cy));
}

fn render_snapshot(
    frame: &mut Frame,
    area: Rect,
    snap: &ScreenSnapshot,
    app: &App,
    mask_cursor_line: bool,
    show_cursor: bool,
) {
    if mask_cursor_line {
        render_vt_screen_with_mask(frame, area, snap, true);
    } else {
        render_vt_screen(frame, area, snap);
    }
    if show_cursor {
        set_cursor_from_snapshot(frame, area, snap);
    }
    render_indicators(frame, area, snap, app);
}

fn split_warp_areas(area: Rect) -> (Rect, Rect) {
    let input_height = 3;
    let pty_height = area.height.saturating_sub(input_height);
    let pty_area = Rect::new(area.x, area.y, area.width, pty_height);
    let input_area = Rect::new(area.x, area.y + pty_height, area.width, input_height);
    (pty_area, input_area)
}

fn labeled_value_line<'a>(label: &'static str, value: Span<'a>) -> Line<'a> {
    Line::from(vec![Span::styled(label, Style::default().fg(DIM)), value])
}

fn selected_row_style(selected: bool) -> (Style, &'static str) {
    if selected {
        (Style::default().bg(super::BG_SELECTED), "›")
    } else {
        (Style::default(), " ")
    }
}

fn selected_agent_accent(app: &App) -> Option<Color> {
    let selected = app.selected_agent()?;
    match selected {
        AgentEntry::Interactive(idx) => app
            .interactive_agents
            .get(*idx)
            .map(|agent| agent.accent_color),
        AgentEntry::Terminal(idx) => app
            .terminal_agents
            .get(*idx)
            .map(|agent| agent.accent_color),
        _ => Some(ACCENT),
    }
}

fn log_panel_border_color(app: &App) -> Color {
    match app.focus {
        Focus::Agent | Focus::Preview => selected_agent_accent(app).unwrap_or(DIM),
        _ => DIM,
    }
}

fn panel_mode_label(app: &App) -> Option<&'static str> {
    match app.focus {
        Focus::Preview => Some(" Preview "),
        Focus::Agent => Some(" Focus "),
        _ => None,
    }
}

fn show_home_fallback(app: &App) -> bool {
    app.agents.is_empty()
        && app.sidebar_mode != SidebarMode::Projects
        && !matches!(
            app.focus,
            Focus::NewAgentDialog
                | Focus::LaunchpadDialog
                | Focus::ContextTransfer
                | Focus::RagTransfer
                | Focus::PromptTemplateDialog
        )
}

fn draw_home_panel(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(brain) = app.home_brain.as_ref() {
        draw_brians_brain(frame, area, brain);
    }
    draw_canopy_banner_animation(frame, area, app);
}

fn draw_log_panel_focus(frame: &mut Frame, area: Rect, app: &mut App) -> bool {
    match app.focus {
        Focus::Home => {
            if app.sidebar_mode == SidebarMode::Projects {
                draw_projects_mode_panel(frame, area, app);
            } else {
                draw_home_panel(frame, area, app);
            }
            true
        }
        Focus::Preview => draw_preview_panel(frame, area, app),
        Focus::Agent => draw_agent_panel(frame, area, app),
        Focus::NewAgentDialog => draw_new_agent_dialog_background(frame, area, app),
        Focus::LaunchpadDialog
        | Focus::ContextTransfer
        | Focus::RagTransfer
        | Focus::PromptTemplateDialog => false,
    }
}

fn draw_preview_panel(frame: &mut Frame, area: Rect, app: &App) -> bool {
    if app.playground_active {
        draw_playground_panel(frame, area, app);
        return true;
    }

    if app.sidebar_mode == SidebarMode::Projects {
        draw_projects_mode_panel(frame, area, app);
        return true;
    }

    if app.agents_rag_focused && app.rag_info.has_rag_activity() {
        draw_rag_info_overview(frame, area, app);
        return true;
    }

    let Some(selected) = app.selected_agent() else {
        return false;
    };

    draw_selected_preview(frame, area, app, selected)
}

fn draw_agent_panel(frame: &mut Frame, area: Rect, app: &mut App) -> bool {
    if app.playground_active {
        draw_playground_panel(frame, area, app);
        return true;
    }

    let Some(selected) = app.selected_agent() else {
        return false;
    };

    match selected {
        AgentEntry::Interactive(idx) => draw_focused_interactive_panel(frame, area, app, *idx),
        AgentEntry::Terminal(idx) => draw_focused_terminal_panel(frame, area, app, *idx),
        AgentEntry::Group(idx) => {
            draw_group_details(frame, area, app, *idx);
            true
        }
        _ => false,
    }
}

fn draw_interactive_preview(frame: &mut Frame, area: Rect, app: &App, idx: usize) -> bool {
    let Some(agent) = app.interactive_agents.get(idx) else {
        return false;
    };
    let Some(snap) = agent.screen_snapshot() else {
        return false;
    };

    render_snapshot(frame, area, &snap, app, false, false);
    true
}

fn draw_terminal_preview(frame: &mut Frame, area: Rect, app: &App, idx: usize) -> bool {
    let Some(agent) = app.terminal_agents.get(idx) else {
        return false;
    };
    let Some(snap) = agent.screen_snapshot() else {
        return false;
    };

    render_snapshot(frame, area, &snap, app, false, false);
    render_command_chips(frame, area, app, &agent.name);
    true
}

fn draw_focused_interactive_panel(frame: &mut Frame, area: Rect, app: &App, idx: usize) -> bool {
    let Some(agent) = app.interactive_agents.get(idx) else {
        return false;
    };
    let Some(snap) = agent.screen_snapshot() else {
        return false;
    };

    render_snapshot(
        frame,
        area,
        &snap,
        app,
        agent.is_sensitive_input_active(),
        false,
    );
    set_focused_interactive_cursor(frame, area, &snap, agent);
    true
}

fn draw_focused_terminal_panel(frame: &mut Frame, area: Rect, app: &mut App, idx: usize) -> bool {
    let Some(agent) = app.terminal_agents.get(idx) else {
        return false;
    };

    let sensitive = agent.is_sensitive_input_active();
    let warp_mode = agent.warp_mode;
    let snap = agent.screen_snapshot();

    if !warp_mode {
        let Some(snap) = snap else {
            return false;
        };
        render_snapshot(frame, area, &snap, app, sensitive, true);
        return true;
    }

    draw_terminal_warp_mode(frame, area, app, idx, snap.as_ref(), sensitive);
    true
}

fn draw_selected_preview(frame: &mut Frame, area: Rect, app: &App, selected: &AgentEntry) -> bool {
    match selected {
        AgentEntry::Agent(agent) => {
            draw_agent_details(frame, area, agent, app);
            true
        }
        AgentEntry::Interactive(idx) => draw_interactive_preview(frame, area, app, *idx),
        AgentEntry::Terminal(idx) => draw_terminal_preview(frame, area, app, *idx),
        AgentEntry::Group(idx) => {
            draw_group_details(frame, area, app, *idx);
            true
        }
    }
}

fn draw_terminal_warp_mode(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    idx: usize,
    snap: Option<&crate::tui::agent::ScreenSnapshot>,
    sensitive: bool,
) {
    let (pty_area, input_area) = split_warp_areas(area);
    if let Some(snap) = snap {
        render_snapshot(frame, pty_area, snap, app, sensitive, false);
    }
    draw_warp_input_box(frame, input_area, app, idx);
    app.last_panel_inner = (pty_area.width, pty_area.height);
    app.last_panel_y = pty_area.y;
}

fn draw_new_agent_dialog_background(frame: &mut Frame, area: Rect, app: &App) -> bool {
    let previous_focus = app
        .new_agent_dialog
        .as_ref()
        .and_then(|dialog| dialog.prev_focus);
    if matches!(previous_focus, Some(Focus::Home) | None) {
        draw_home_panel(frame, area, app);
        return true;
    }

    false
}

fn set_focused_interactive_cursor(
    frame: &mut Frame,
    area: Rect,
    snap: &crate::tui::agent::ScreenSnapshot,
    agent: &crate::tui::agent::InteractiveAgent,
) {
    if snap.scrolled || area.width == 0 || area.height == 0 {
        return;
    }

    let cursor_col = adjusted_interactive_cursor_col(agent.cli.as_str(), snap);
    let cx = area.x + cursor_col.min(area.width.saturating_sub(1));
    let cy = area.y + snap.cursor_row.min(area.height.saturating_sub(1));
    frame.set_cursor_position((cx, cy));
}

fn adjusted_interactive_cursor_col(
    cli_name: &str,
    snap: &crate::tui::agent::ScreenSnapshot,
) -> u16 {
    let cursor_col = snap.cursor_col;
    if !cli_name.to_ascii_lowercase().contains("copilot") {
        return cursor_col;
    }

    // Copilot can render its own in-band cursor decoration; when present, trust
    // the inverse-highlighted cell rather than vt cursor coordinates.
    if let Some(row) = snap.cells.get(snap.cursor_row as usize) {
        if let Some((idx, _)) = row
            .iter()
            .enumerate()
            .find(|(_, cell)| cell.as_ref().is_some_and(|c| c.inverse))
        {
            return idx as u16;
        }
    }

    // Legacy offset fallback for Copilot prompts that report one cell ahead.
    cursor_col.saturating_sub(1)
}

pub(super) fn draw_log_panel(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let border_color = log_panel_border_color(app);
    let title = panel_mode_label(app).map(|label| {
        Span::styled(
            label,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )
    });
    let inner = render_panel_block(frame, area, border_color, title);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    app.last_panel_inner = (inner.width, inner.height);
    app.last_panel_y = inner.y;

    if show_home_fallback(app) {
        draw_home_panel(frame, inner, app);
        return;
    }

    if draw_log_panel_focus(frame, inner, app) {
        return;
    }

    draw_log_text(frame, area, inner, app);
}

fn draw_project_overview(frame: &mut Frame, area: Rect, app: &App) {
    let Some(project) = app.selected_project() else {
        frame.render_widget(
            Paragraph::new("No registered projects").style(Style::default().fg(DIM)),
            area,
        );
        return;
    };

    let tags = project.tags.as_deref().unwrap_or("none");
    let indexed = project
        .indexed_at
        .map(|timestamp| timestamp.to_string())
        .unwrap_or_else(|| "pending".to_string());
    let description = project
        .description
        .as_deref()
        .unwrap_or("No description extracted yet.");
    let lines = vec![
        Line::from(vec![
            Span::styled("Project ", Style::default().fg(DIM)),
            Span::styled(
                &project.name,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format!("Hash: {}", project.hash)),
        Line::from(format!("Path: {}", project.path)),
        Line::from(format!("Indexed: {}", indexed)),
        Line::from(format!("Tags: {}", tags)),
        Line::from(""),
        Line::from(description),
    ];
    render_wrapped_paragraph(frame, area, lines);
}

fn draw_projects_mode_panel(frame: &mut Frame, area: Rect, app: &App) {
    if app.playground_active {
        draw_playground_panel(frame, area, app);
        return;
    }

    match app.projects_panel_focus {
        ProjectsPanelFocus::Projects => draw_project_overview(frame, area, app),
        ProjectsPanelFocus::RagInfo => draw_rag_queue_overview(frame, area, app),
    }
}

fn draw_rag_queue_overview(frame: &mut Frame, area: Rect, app: &App) {
    draw_rag_info_overview(frame, area, app);
}

fn rag_status(app: &App) -> (&'static str, Color) {
    if app.rag_paused {
        ("⏸ paused", Color::Yellow)
    } else if app.rag_info.processing_items > 0 {
        ("◉ indexing", Color::Yellow)
    } else if app.rag_info.queued_items > 0 {
        ("⏳ pending", Color::Yellow)
    } else {
        ("✓ ready", ACCENT)
    }
}

fn rag_queue_text(app: &App) -> String {
    if app.rag_info.queued_items > 0 {
        format!("{} queued", app.rag_info.queued_items)
    } else {
        String::new()
    }
}

fn rag_summary_lines(
    app: &App,
    status_text: &'static str,
    status_color: Color,
    queue_text: String,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Chunks: ", Style::default().fg(DIM)),
            Span::styled(
                app.rag_info.total_chunks.to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Files: ", Style::default().fg(DIM)),
            Span::styled(
                app.rag_info.indexed_files.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        labeled_value_line(
            "Status: ",
            Span::styled(status_text, Style::default().fg(status_color)),
        ),
    ];
    if !queue_text.is_empty() {
        lines.push(labeled_value_line(
            "Queue:  ",
            Span::styled(queue_text, Style::default().fg(Color::White)),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Enter to open the global RAG playground.",
        Style::default().fg(ACCENT),
    )));
    lines
}

fn draw_rag_info_overview(frame: &mut Frame, area: Rect, app: &App) {
    let (status_text, status_color) = rag_status(app);
    let mut lines = rag_summary_lines(app, status_text, status_color, rag_queue_text(app));

    if !app.rag_file_status.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Recent files  ", Style::default().fg(DIM)),
            Span::styled(
                format!("({}) ", app.rag_file_status.len()),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.extend(rag_file_status_lines(&app.rag_file_status, area));
    } else if !app.global_rag_queue.is_empty() {
        lines.extend(rag_queue_lines(
            &app.global_rag_queue,
            app.selected_rag_queue,
        ));
    }

    render_wrapped_paragraph(frame, area, lines);
}

fn rag_file_status_lines(
    files: &[crate::db::project::RagPerFileStatus],
    area: Rect,
) -> Vec<Line<'static>> {
    // Reserve summary/header space and render a fixed non-interactive preview window.
    let max_rows = (area.height as usize).saturating_sub(8).max(2);
    let name_width = area.width.saturating_sub(4) as usize;
    let detail_width = area.width.saturating_sub(6) as usize;

    let mut lines = Vec::new();
    for file in files.iter().take(max_rows) {
        let (icon, icon_color) = match file.last_event_type.as_str() {
            "indexed" => ("✓", Color::Green),
            "deleted" => ("○", DIM),
            _ => ("✗", Color::Red),
        };
        let filename = std::path::Path::new(&file.file_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file.file_path.clone());
        let name_trunc = truncate_str(&filename, name_width);
        let detail = if file.last_event_type == "error" {
            file.last_detail
                .as_deref()
                .map(|d| format!("error: {}", truncate_str_keep_tail(d, detail_width)))
                .unwrap_or_else(|| "error".to_string())
        } else if file.last_event_type == "deleted" {
            "deleted".to_string()
        } else {
            format!("indexed ×{}", file.times_indexed)
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
            Span::styled(
                name_trunc,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default().fg(DIM)),
            Span::styled(detail, Style::default().fg(DIM)),
        ]));
    }

    if max_rows < files.len() {
        let remaining = files.len() - max_rows;
        lines.push(Line::from(Span::styled(
            format!("  … {} more (open playground for full details)", remaining),
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn rag_queue_lines(
    queue: &[crate::db::project::RagQueueItem],
    selected: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Queue items ", Style::default().fg(DIM)),
            Span::styled(
                format!("({})", queue.len()),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    for (idx, item) in queue.iter().enumerate().take(8) {
        let (line_style, marker) = selected_row_style(idx == selected);
        let status_color = if item.status == "processing" {
            Color::Yellow
        } else {
            ACCENT
        };
        lines.push(Line::from(vec![
            Span::styled(marker, line_style.fg(status_color)),
            Span::raw(" "),
            Span::styled(
                truncate_str(&item.source_path, 40),
                line_style.fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", item.status), line_style.fg(DIM)),
        ]));
    }

    lines
}

fn draw_playground_panel(frame: &mut Frame, area: Rect, app: &App) {
    if app.playground_detail_mode {
        draw_playground_detail(frame, area, app);
    } else {
        draw_playground_list(frame, area, app);
    }
}

fn playground_header_lines(scope_label: &str, query: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("RAG Playground ", Style::default().fg(DIM)),
            Span::styled(format!("({scope_label}) "), Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("· {query}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "Type to search · ↑↓ navigate · Tab toggle scope · Enter focus · Ctrl+T transfer · Esc close",
            Style::default().fg(DIM),
        )),
        Line::from(""),
    ]
}

fn playground_empty_state(query: &str) -> Line<'static> {
    let message = if query.trim().is_empty() {
        "Start typing to search indexed chunks."
    } else {
        "No matching chunks."
    };
    Line::from(Span::styled(message, Style::default().fg(DIM)))
}

fn visible_playground_window(area: Rect, selected: usize) -> (usize, usize) {
    let max_visible = ((area.height.saturating_sub(4)) / 5).max(1) as usize;
    let scroll_start = if selected >= max_visible {
        selected.saturating_sub(max_visible - 1)
    } else {
        0
    };
    (max_visible, scroll_start)
}

fn project_name_for_chunk<'a>(
    app: &'a App,
    _chunk: &crate::rag::vector_store::SearchResult,
) -> &'a str {
    app.projects
        .first()
        .map(|project| project.name.as_str())
        .unwrap_or("?")
}

fn draw_playground_list(frame: &mut Frame, area: Rect, app: &App) {
    let scope_label = playground_scope_label(app);
    let mut lines = playground_header_lines(&scope_label, &app.playground_query);

    if app.playground_results.is_empty() {
        lines.push(playground_empty_state(&app.playground_query));
        render_wrapped_paragraph(frame, area, lines);
        return;
    }

    let total = app.playground_results.len();
    let (max_visible, scroll_start) = visible_playground_window(area, app.playground_selected);
    for (idx, chunk) in app
        .playground_results
        .iter()
        .enumerate()
        .skip(scroll_start)
        .take(max_visible)
    {
        lines.extend(render_chunk_entry(
            chunk,
            project_name_for_chunk(app, chunk),
            idx == app.playground_selected,
            area.width,
        ));
    }

    if total > max_visible {
        lines.push(Line::from(Span::styled(
            format!("  {}/{} results", app.playground_selected + 1, total),
            Style::default().fg(DIM),
        )));
    }

    render_wrapped_paragraph(frame, area, lines);
}

fn playground_scope_label(app: &App) -> String {
    app.playground_project_hash
        .as_ref()
        .and_then(|hash| app.projects.iter().find(|p| &p.hash == hash))
        .map(|p| format!("Project: {}", p.name))
        .unwrap_or_else(|| "Global".to_string())
}

fn render_chunk_entry<'a>(
    chunk: &'a crate::rag::vector_store::SearchResult,
    project_name: &'a str,
    selected: bool,
    width: u16,
) -> Vec<Line<'a>> {
    let (style, marker) = selected_row_style(selected);
    let dist = chunk
        .distance
        .map_or("—".to_string(), |d| format!("{d:.3}"));
    let path = format!("{} · {} [dist={}]", project_name, chunk.file_path, dist);
    let mut lines = vec![Line::from(vec![
        Span::styled(marker, style.fg(ACCENT)),
        Span::raw(" "),
        Span::styled(
            truncate_str(&path, width.saturating_sub(3) as usize),
            style.fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ])];

    for line in chunk.content.lines().take(3) {
        lines.push(Line::from(vec![
            Span::styled("   ", style),
            Span::styled(
                truncate_str(line, width.saturating_sub(6) as usize),
                style.fg(DIM),
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines
}

fn playground_detail_header(chunk: &crate::rag::vector_store::SearchResult) -> Vec<Line<'static>> {
    let dist = chunk
        .distance
        .map_or("—".to_string(), |d| format!("{d:.4}"));
    vec![
        Line::from(vec![
            Span::styled("‹ ", Style::default().fg(ACCENT)),
            Span::styled(
                format!("{} [dist={}]", chunk.file_path, dist),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "↑↓ scroll · Enter/Ctrl+T transfer · Esc back to list",
            Style::default().fg(DIM),
        )),
        Line::from(""),
    ]
}

fn detail_progress_line(
    total_lines: usize,
    visible_lines: usize,
    start: usize,
) -> Option<Line<'static>> {
    if total_lines <= visible_lines {
        return None;
    }

    let percent = if total_lines == 0 {
        100
    } else {
        ((start + visible_lines).min(total_lines) * 100 / total_lines).min(100)
    };
    Some(Line::from(Span::styled(
        format!("  ── {percent}% ──"),
        Style::default().fg(DIM),
    )))
}

fn draw_playground_detail(frame: &mut Frame, area: Rect, app: &App) {
    let Some(chunk) = app.playground_results.get(app.playground_selected) else {
        return;
    };

    let mut lines = playground_detail_header(chunk);
    let content_lines: Vec<&str> = chunk.content.lines().collect();
    let visible_lines = area.height.saturating_sub(5) as usize;
    let start = (app.playground_scroll as usize).min(content_lines.len().saturating_sub(1));
    let end = (start + visible_lines).min(content_lines.len());

    for line in &content_lines[start..end] {
        lines.push(Line::from(Span::styled(
            truncate_str(line, area.width as usize),
            Style::default().fg(Color::White),
        )));
    }

    if let Some(progress) = detail_progress_line(content_lines.len(), visible_lines, start) {
        lines.push(Line::from(""));
        lines.push(progress);
    }

    render_wrapped_paragraph(frame, area, lines);
}

// ── Split panel ─────────────────────────────────────────────────

/// Render one half of a split view — finds the session by name and draws its PTY.
pub(super) fn draw_split_panel(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    session_name: &str,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let found = find_session_by_name(app, session_name);
    let border_color = if focused {
        found.map_or(DIM, |session| session.accent(app))
    } else {
        DIM
    };
    let title = Span::styled(
        if focused {
            format!(" ● {session_name} ")
        } else {
            format!("   {session_name} ")
        },
        Style::default()
            .fg(border_color)
            .add_modifier(Modifier::BOLD),
    );

    let inner = render_panel_block(frame, area, border_color, Some(title));
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if focused {
        app.last_panel_inner = (inner.width, inner.height);
        app.last_panel_y = inner.y;
    }

    let Some(session) = found else {
        render_missing_session(frame, inner, session_name);
        return;
    };

    if let Some(terminal_idx) = session.warp_terminal_idx(app) {
        let snapshot = session.snapshot(app);
        draw_split_warp_panel(frame, inner, app, terminal_idx, snapshot.as_ref(), focused);
        return;
    }

    let Some(snap) = session.snapshot(app) else {
        return;
    };

    render_snapshot(
        frame,
        inner,
        &snap,
        app,
        false,
        focused && matches!(app.focus, Focus::Agent),
    );
}

fn draw_split_warp_panel(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    terminal_idx: usize,
    snap: Option<&ScreenSnapshot>,
    focused: bool,
) {
    let (pty_area, input_area) = split_warp_areas(area);

    if let Some(snap) = snap {
        render_snapshot(frame, pty_area, snap, app, false, false);
    }

    if focused && matches!(app.focus, Focus::Agent) {
        draw_warp_input_box(frame, input_area, app, terminal_idx);
    }

    if focused {
        app.last_panel_inner = (pty_area.width, pty_area.height);
        app.last_panel_y = pty_area.y;
    }
}

fn render_missing_session(frame: &mut Frame, area: Rect, session_name: &str) {
    let message = Paragraph::new(format!("  Session '{session_name}' not found"))
        .style(Style::default().fg(DIM));
    frame.render_widget(message, area);
}

#[derive(Clone, Copy)]
enum SessionRef {
    Interactive(usize),
    Terminal(usize),
}

impl SessionRef {
    fn accent(self, app: &App) -> Color {
        match self {
            SessionRef::Interactive(idx) => app.interactive_agents[idx].accent_color,
            SessionRef::Terminal(idx) => app.terminal_agents[idx].accent_color,
        }
    }

    fn snapshot(self, app: &App) -> Option<ScreenSnapshot> {
        match self {
            SessionRef::Interactive(idx) => app.interactive_agents[idx].screen_snapshot(),
            SessionRef::Terminal(idx) => app.terminal_agents[idx].screen_snapshot(),
        }
    }

    fn warp_terminal_idx(self, app: &App) -> Option<usize> {
        match self {
            SessionRef::Terminal(idx) if app.terminal_agents[idx].warp_mode => Some(idx),
            _ => None,
        }
    }
}

fn find_session_by_name(app: &App, name: &str) -> Option<SessionRef> {
    if let Some(idx) = app
        .interactive_agents
        .iter()
        .position(|agent| agent.name == name)
    {
        return Some(SessionRef::Interactive(idx));
    }
    if let Some(idx) = app
        .terminal_agents
        .iter()
        .position(|agent| agent.name == name)
    {
        return Some(SessionRef::Terminal(idx));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::adjusted_interactive_cursor_col;
    use crate::tui::agent::screen::VtCell;
    use crate::tui::agent::ScreenSnapshot;
    use ratatui::style::Color;

    #[test]
    fn copilot_cursor_is_shifted_left_by_one() {
        let snap = ScreenSnapshot {
            cells: vec![(0..8).map(|_| None).collect()],
            cursor_row: 0,
            cursor_col: 5,
            scrolled: false,
        };
        assert_eq!(adjusted_interactive_cursor_col("copilot", &snap), 4);
        let snap_zero = ScreenSnapshot {
            cursor_col: 0,
            ..snap
        };
        assert_eq!(adjusted_interactive_cursor_col("copilot", &snap_zero), 0);
    }

    #[test]
    fn copilot_cursor_prefers_inverse_cell_when_present() {
        let mut row: Vec<Option<VtCell>> = (0..8).map(|_| None).collect();
        row[3] = Some(VtCell {
            ch: "x".to_string(),
            fg: Color::White,
            bg: Color::Black,
            bold: false,
            underline: false,
            inverse: true,
        });
        let snap = ScreenSnapshot {
            cells: vec![row],
            cursor_row: 0,
            cursor_col: 7,
            scrolled: false,
        };
        assert_eq!(adjusted_interactive_cursor_col("copilot-cli", &snap), 3);
    }

    #[test]
    fn other_clients_keep_their_cursor_position() {
        let snap = ScreenSnapshot {
            cells: vec![(0..8).map(|_| None).collect()],
            cursor_row: 0,
            cursor_col: 5,
            scrolled: false,
        };
        assert_eq!(adjusted_interactive_cursor_col("opencode", &snap), 5);
    }
}
