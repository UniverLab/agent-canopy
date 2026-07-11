//! Right panel rendering — PTY output, brain automaton, banner, background_agent/watcher details, log.

use chrono::{Local, TimeZone};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::{
    truncate_str, truncate_str_keep_tail, ACCENT, BORDER_COLOR, DIM, INTERACTIVE_COLOR,
    STATUS_DISABLED, STATUS_FAIL, STATUS_OK, STATUS_RUNNING,
};
use crate::tui::agent::ScreenSnapshot;
use crate::tui::app::types::{AgentEntry, App, Focus, ProjectsPanelFocus};
use crate::tui::app::SidebarMode;

pub mod background_agent;
pub mod details;
pub mod home;
pub mod log_fallback;
pub mod sync;
pub mod vt100;
pub mod warp;

pub(crate) use background_agent::draw_background_agent_panel;
pub use details::{draw_agent_details, draw_group_details};
pub(crate) use home::draw_brians_brain;
pub use log_fallback::draw_log_text;
pub(crate) use sync::draw_activity_panel;
use vt100::render_vt_screen;
#[allow(unused_imports)]
pub use warp::compact_cwd;
pub use warp::{draw_warp_input_box, render_command_chips};

use home::draw_canopy_banner_animation;
use vt100::render_indicators;

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

fn format_unix_timestamp(timestamp: i64) -> String {
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(datetime) => datetime.format("%Y-%m-%d %H:%M").to_string(),
        None => timestamp.to_string(),
    }
}

fn project_metadata_matches(
    metadata: Option<&str>,
    project: &crate::domain::project::Project,
) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return false;
    };
    value.get("workdir").and_then(serde_json::Value::as_str) == Some(project.path.as_str())
}

fn recent_project_session_summaries(
    app: &App,
    project: &crate::domain::project::Project,
    limit: usize,
) -> Vec<(String, String)> {
    let Ok(nodes) =
        app.db
            .search_intelligence_nodes(&project.path, Some("session"), limit.saturating_mul(4))
    else {
        return Vec::new();
    };

    nodes
        .into_iter()
        .filter(|node| {
            node.project_hash.as_deref() == Some(project.hash.as_str())
                || project_metadata_matches(node.metadata.as_deref(), project)
        })
        .take(limit)
        .map(|node| {
            let summary = if node.body.trim().is_empty() {
                "No summary captured yet.".to_string()
            } else {
                truncate_str(node.body.trim(), 96)
            };
            (node.title, summary)
        })
        .collect()
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
    _mask_cursor_line: bool,
    show_cursor: bool,
    selection: Option<vt100::PaneSelection>,
) {
    render_vt_screen(frame, area, snap, selection);
    if show_cursor {
        set_cursor_from_snapshot(frame, area, snap);
    }
    render_indicators(frame, area, snap, app);
}

/// The active mouse selection, if it belongs to the pane's agent.
fn pane_selection(app: &App, is_terminal: bool, idx: usize) -> Option<vt100::PaneSelection> {
    let sel = app.terminal_selection?;
    (sel.agent == (is_terminal, idx)).then(|| sel.normalized())
}

/// Splits the terminal-warp panel into the PTY output area and the input
/// box, with a 1-row gap between them. `input_text` is the current buffer
/// contents (used to size the input box for wrapped/multiline content).
fn split_warp_areas(area: Rect, input_text: &str) -> (Rect, Rect) {
    let input_height = warp::input_height(input_text, area.width);
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(input_height),
    ])
    .split(area);
    (chunks[0], chunks[2])
}

fn warp_input_text(app: &App, idx: usize) -> String {
    app.terminal_agents
        .get(idx)
        .map(|agent| {
            if agent.is_sensitive_input_active() {
                String::new()
            } else {
                agent
                    .input_buffer
                    .lock()
                    .map(|b| b.clone())
                    .unwrap_or_default()
            }
        })
        .unwrap_or_default()
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
        Focus::Agent | Focus::Preview => selected_agent_accent(app).unwrap_or(BORDER_COLOR),
        _ => BORDER_COLOR,
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
                | Focus::LoopEditorDialog
                | Focus::LoopFormDialog
                | Focus::ProjectRelationDialog
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
        | Focus::KnowledgeDialog
        | Focus::ContextTransfer
        | Focus::RagTransfer
        | Focus::PromptTemplateDialog
        | Focus::LoopEditorDialog
        | Focus::LoopFormDialog => false,
        Focus::ProjectRelationDialog => {
            draw_projects_mode_panel(frame, area, app);
            true
        }
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
        AgentEntry::Agent(agent) => {
            draw_background_agent_panel(frame, area, agent, app);
            true
        }
        AgentEntry::Orphaned(idx) => {
            if let Some(session) = app.orphaned_sessions.get(*idx) {
                let text = format!(
                    "Orphaned session: {}\nCLI: {}  Workdir: {}\n\nPress 'r' to revive or 'd' to dismiss.",
                    session.name, session.cli, session.working_dir
                );
                let paragraph = ratatui::widgets::Paragraph::new(text)
                    .style(ratatui::style::Style::default().fg(ratatui::style::Color::Yellow));
                frame.render_widget(paragraph, area);
            }
            true
        }
    }
}

fn draw_interactive_preview(frame: &mut Frame, area: Rect, app: &App, idx: usize) -> bool {
    let Some(agent) = app.interactive_agents.get(idx) else {
        return false;
    };
    let Some(snap) = agent.screen_snapshot() else {
        return false;
    };

    render_snapshot(frame, area, &snap, app, false, false, None);
    true
}

fn draw_terminal_preview(frame: &mut Frame, area: Rect, app: &App, idx: usize) -> bool {
    let Some(agent) = app.terminal_agents.get(idx) else {
        return false;
    };
    let Some(snap) = agent.screen_snapshot() else {
        return false;
    };

    render_snapshot(frame, area, &snap, app, false, false, None);
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
        pane_selection(app, false, idx),
    );
    set_focused_interactive_cursor(frame, area, &snap, agent);
    true
}

fn draw_focused_terminal_panel(frame: &mut Frame, area: Rect, app: &mut App, idx: usize) -> bool {
    let Some(agent) = app.terminal_agents.get(idx) else {
        return false;
    };

    let sensitive = agent.is_sensitive_input_active();
    // Warp input box only while the shell itself owns the terminal; when a
    // wizard/TUI/foreground command is running the PTY gets the whole pane.
    let warp_active = agent.warp_mode && !agent.should_bypass_warp_input();
    let snap = agent.screen_snapshot();

    if !warp_active {
        let Some(snap) = snap else {
            return false;
        };
        render_snapshot(
            frame,
            area,
            &snap,
            app,
            sensitive,
            true,
            pane_selection(app, true, idx),
        );
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
        AgentEntry::Orphaned(idx) => {
            if let Some(session) = app.orphaned_sessions.get(*idx) {
                let text = format!(
                    "Orphaned: {} ({})\nWorkdir: {}",
                    session.name, session.cli, session.working_dir
                );
                let paragraph = ratatui::widgets::Paragraph::new(text)
                    .style(ratatui::style::Style::default().fg(ratatui::style::Color::Yellow));
                frame.render_widget(paragraph, area);
            }
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
    _sensitive: bool,
) {
    let (pty_area, input_area) = split_warp_areas(area, &warp_input_text(app, idx));
    if let Some(snap) = snap {
        render_snapshot(
            frame,
            pty_area,
            snap,
            app,
            false,
            false,
            pane_selection(app, true, idx),
        );
    }
    draw_warp_input_box(frame, input_area, app, idx);
    app.last_panel_inner = (pty_area.width, pty_area.height);
    app.last_panel_x = pty_area.x;
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

    // Copilot renders its own in-band cursor as an inverse-highlighted cell.
    // Trust that decoration over the vt cursor coordinates.
    if let Some(row) = snap.cells.get(snap.cursor_row as usize) {
        if let Some((idx, _)) = row
            .iter()
            .enumerate()
            .find(|(_, cell)| cell.as_ref().is_some_and(|c| c.inverse))
        {
            return idx as u16;
        }
    }

    cursor_col
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
    app.last_panel_x = inner.x;
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

fn format_intent_lines(
    state: &crate::tui::app::types::SyncPanelState,
    _area_width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if state.active_intents.is_empty() {
        lines.push(Line::from("active missions: none"));
        return lines;
    }

    lines.push(Line::from("active missions:"));
    for intent in state.active_intents.iter().take(3) {
        lines.push(Line::from(format!(
            "  - {} [{}]: {}",
            intent.agent_name,
            intent.impact.as_str(),
            intent.mission
        )));
        if !intent.description.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    {}", truncate_str(intent.description.trim(), 92)),
                Style::default().fg(DIM),
            )));
        }
    }
    lines
}

fn format_recent_activity_lines(
    state: &crate::tui::app::types::SyncPanelState,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let recent_messages = state
        .recent_messages
        .iter()
        .rev()
        .take(3)
        .collect::<Vec<_>>();

    if recent_messages.is_empty() {
        lines.push(Line::from("recent activity: none"));
        return lines;
    }

    lines.push(Line::from("recent activity:"));
    for message in recent_messages {
        lines.push(Line::from(format!(
            "  - {}: {}",
            message.agent_name,
            truncate_str(message.message.trim(), 92)
        )));
    }
    lines
}

fn format_recent_session_lines(sessions: &[(String, String)]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if sessions.is_empty() {
        lines.push(Line::from("  none"));
        return lines;
    }

    for (title, summary) in sessions {
        lines.push(Line::from(format!("  - {}", truncate_str(title, 92))));
        lines.push(Line::from(Span::styled(
            format!("    {}", summary),
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn build_project_overview_lines<'a>(
    project: &'a crate::domain::project::Project,
    project_activity: Option<&crate::tui::app::types::SyncPanelState>,
    recent_sessions: &[(String, String)],
) -> Vec<Line<'a>> {
    let tags = project.tags.as_deref().unwrap_or("none");
    let indexed = project
        .indexed_at
        .map(format_unix_timestamp)
        .unwrap_or_else(|| "pending".to_string());
    let created = format_unix_timestamp(project.created_at);
    let description = project
        .description
        .as_deref()
        .unwrap_or("No description extracted yet.");

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Project ", Style::default().fg(DIM)),
            Span::styled(
                &project.name,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format!("workdir_hash: {}", project.hash)),
        Line::from(format!("path: {}", project.path)),
        Line::from(format!("indexed_at: {}", indexed)),
        Line::from(format!("created_at: {}", created)),
        Line::from(format!("tags: {}", tags)),
        Line::from(""),
        Line::from(Span::styled("description", Style::default().fg(DIM))),
        Line::from(description),
        Line::from(""),
        Line::from(Span::styled("workspace context", Style::default().fg(DIM))),
    ];

    if let Some(state) = project_activity {
        lines.push(Line::from(format!(
            "participants: {}  vibe: {}",
            state.participant_count,
            state.vibe.as_str()
        )));
        lines.extend(format_intent_lines(state, 0));
        lines.extend(format_recent_activity_lines(state));
    } else {
        lines.push(Line::from("participants: 0  vibe: stable"));
        lines.push(Line::from("active missions: none"));
        lines.push(Line::from("recent activity: none"));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "recent sessions",
        Style::default().fg(DIM),
    )));
    lines.extend(format_recent_session_lines(recent_sessions));

    lines
}

fn draw_project_overview(frame: &mut Frame, area: Rect, app: &App) {
    let Some(project) = app.selected_project() else {
        frame.render_widget(
            Paragraph::new("No registered projects").style(Style::default().fg(DIM)),
            area,
        );
        return;
    };

    let project_activity = app.activity_panel_state_for_workdir(&project.path);
    let recent_sessions = recent_project_session_summaries(app, project, 3);
    let lines = build_project_overview_lines(project, project_activity.as_ref(), &recent_sessions);

    render_wrapped_paragraph(frame, area, lines);
}

fn draw_projects_mode_panel(frame: &mut Frame, area: Rect, app: &App) {
    if app.playground_active {
        draw_playground_panel(frame, area, app);
        return;
    }

    match app.projects_panel_focus {
        ProjectsPanelFocus::Projects => draw_project_overview(frame, area, app),
        ProjectsPanelFocus::Loops => draw_loop_overview(frame, area, app),
        ProjectsPanelFocus::Knowledge => draw_knowledge_overview(frame, area, app),
        ProjectsPanelFocus::RagInfo => draw_rag_queue_overview(frame, area, app),
    }
}

fn draw_knowledge_overview(frame: &mut Frame, area: Rect, app: &App) {
    if app.project_knowledge.is_empty() {
        frame.render_widget(
            Paragraph::new("No knowledge yet. Agents can add facts/patterns.")
                .style(Style::default().fg(DIM)),
            area,
        );
        return;
    }

    let Some(node) = app.project_knowledge.get(app.selected_knowledge) else {
        frame.render_widget(
            Paragraph::new("No knowledge selected").style(Style::default().fg(DIM)),
            area,
        );
        return;
    };

    let kind_color = if node.kind == "fact" {
        Color::Cyan
    } else {
        Color::Magenta
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Knowledge ", Style::default().fg(DIM)),
            Span::styled(
                node.title.as_str(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(format!("[{}]", node.kind), Style::default().fg(kind_color)),
        ]),
        Line::from(""),
    ];

    for line in node.body.lines() {
        lines.push(Line::from(Span::styled(
            line,
            Style::default().fg(Color::White),
        )));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn draw_rag_queue_overview(frame: &mut Frame, area: Rect, app: &App) {
    draw_rag_info_overview(frame, area, app);
}

fn draw_loop_overview(frame: &mut Frame, area: Rect, app: &App) {
    let Some(lp) = app.selected_loop() else {
        frame.render_widget(
            Paragraph::new("No loops yet").style(Style::default().fg(DIM)),
            area,
        );
        return;
    };
    let Some(details) = app.selected_loop_details() else {
        frame.render_widget(
            Paragraph::new("Loop details are unavailable").style(Style::default().fg(DIM)),
            area,
        );
        return;
    };
    let Some(spec) = app.selected_loop_spec() else {
        frame.render_widget(
            Paragraph::new("Loop has no specs yet").style(Style::default().fg(DIM)),
            area,
        );
        return;
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Loop ", Style::default().fg(DIM)),
            Span::styled(
                lp.name.as_str(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                lp.status.as_str().to_uppercase(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(format!("Workdir: {}", lp.workdir)),
        Line::from(format!(
            "Spec {}/{}: {} [{}]",
            app.loop_selected_spec + 1,
            details.specs.len(),
            spec.spec.name,
            spec.spec.status.as_str()
        )),
        Line::from(Span::styled(
            "Tab section  ·  [ ] spec  ·  ←→ node  ·  Enter/e edit",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled("Graph", Style::default().fg(DIM))),
    ];

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Graph", Style::default().fg(DIM))));

    if spec.nodes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no nodes yet)",
            Style::default().fg(DIM),
        )));
    } else {
        lines.extend(loop_graph_lines(spec, app.loop_selected_node, area.width));
    }

    if !app.loop_runs.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Recent runs",
            Style::default().fg(DIM),
        )));
        lines.extend(app.loop_runs.iter().rev().take(4).map(|run| {
            Line::from(format!(
                "  iter {}  {}  {}",
                run.iteration,
                run.node_id,
                run.status.as_str()
            ))
        }));
    }

    render_wrapped_paragraph(frame, area, lines);
}

fn loop_node_summary(node: &crate::domain::loops::LoopNode) -> String {
    match node.kind {
        crate::domain::loops::LoopNodeKind::Agent => node
            .config
            .get("prompt_template")
            .and_then(serde_json::Value::as_str)
            .map(|prompt| truncate_str(prompt, 72))
            .filter(|prompt| !prompt.is_empty())
            .unwrap_or_else(|| "prompt_template not set".to_string()),
        crate::domain::loops::LoopNodeKind::Check => node
            .config
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(|command| format!("check: {}", truncate_str(command, 72)))
            .unwrap_or_else(|| "check config".to_string()),
        crate::domain::loops::LoopNodeKind::Gate => {
            let evaluate = node
                .config
                .get("evaluate")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("gate");
            let value = node
                .config
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("{evaluate}: {}", truncate_str(value, 48))
        }
    }
}

fn loop_node_box_styles(selected: bool) -> (Style, Style) {
    if selected {
        (
            Style::default().fg(ACCENT),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::default().fg(BORDER_COLOR),
            Style::default().fg(Color::White),
        )
    }
}

fn loop_node_content_line(
    node: &crate::domain::loops::LoopNode,
    selected: bool,
    inner: usize,
) -> Option<Line<'static>> {
    let summary = loop_node_summary(node);
    if summary.is_empty() {
        return None;
    }

    let summary_trunc = truncate_str(&summary, inner.saturating_sub(3));
    let summary_pad = inner.saturating_sub(3 + summary_trunc.len());
    let summary_line = format!("  │   {}{}│", summary_trunc, " ".repeat(summary_pad));
    Some(Line::from(Span::styled(
        summary_line,
        if selected {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(DIM)
        },
    )))
}

fn loop_node_lines(
    node: &crate::domain::loops::LoopNode,
    selected: bool,
    inner: usize,
) -> Vec<Line<'static>> {
    let (border_style, text_style) = loop_node_box_styles(selected);
    let kind_tag = format!("[{}]", node.kind.as_str());
    let max_name = inner.saturating_sub(2 + kind_tag.len());
    let name_display = truncate_str(&node.name, max_name);
    let spaces = inner.saturating_sub(2 + name_display.len() + kind_tag.len());
    let marker = if selected { "›" } else { " " };

    let mut lines = vec![
        Line::from(Span::styled(
            format!("  ┌{}┐", "─".repeat(inner)),
            border_style,
        )),
        Line::from(Span::styled(
            format!(
                "  │{} {}{}{}│",
                marker,
                name_display,
                " ".repeat(spaces),
                kind_tag
            ),
            text_style,
        )),
    ];

    if let Some(content) = loop_node_content_line(node, selected, inner) {
        lines.push(content);
    }

    lines.push(Line::from(Span::styled(
        format!("  └{}┘", "─".repeat(inner)),
        border_style,
    )));

    lines
}

fn loop_edge_lines(
    edges: &[(usize, crate::domain::loops::LoopEdgeCondition)],
    spec_nodes: &[crate::domain::loops::LoopNode],
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, (target_idx, condition)) in edges.iter().enumerate() {
        let branch = if i == edges.len() - 1 { "└" } else { "├" };
        let target_name = spec_nodes
            .get(*target_idx)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| "?".to_string());
        lines.push(Line::from(Span::styled(
            format!("   {}─ {} → {}", branch, condition.as_str(), target_name),
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn loop_graph_lines(
    spec: &crate::domain::loops::LoopSpecDetails,
    selected_node_idx: usize,
    area_width: u16,
) -> Vec<Line<'static>> {
    use std::collections::HashMap;

    let mut outgoing: HashMap<&str, Vec<(usize, crate::domain::loops::LoopEdgeCondition)>> =
        HashMap::new();
    for edge in &spec.edges {
        if let Some(target_idx) = spec.nodes.iter().position(|n| n.id == edge.to_node) {
            outgoing
                .entry(edge.from_node.as_str())
                .or_default()
                .push((target_idx, edge.condition));
        }
    }

    let box_width = (area_width as usize).saturating_sub(4).clamp(22, 48);
    let inner = box_width.saturating_sub(2);

    let mut lines: Vec<Line<'static>> = Vec::new();

    for (idx, node) in spec.nodes.iter().enumerate() {
        let selected = idx == selected_node_idx;
        lines.extend(loop_node_lines(node, selected, inner));

        if let Some(edges) = outgoing.get(node.id.as_str()) {
            lines.extend(loop_edge_lines(edges, &spec.nodes));
            lines.push(Line::from(""));
        } else if idx < spec.nodes.len() - 1 {
            lines.push(Line::from(""));
        }
    }

    lines
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

fn rag_file_icon_and_color(event_type: &str) -> (&'static str, Color) {
    match event_type {
        "indexed" => ("✓", Color::Green),
        "deleted" => ("○", DIM),
        _ => ("✗", Color::Red),
    }
}

fn rag_file_detail(file: &crate::db::project::RagPerFileStatus, detail_width: usize) -> String {
    if file.last_event_type == "error" {
        file.last_detail
            .as_deref()
            .map(|d| format!("error: {}", truncate_str_keep_tail(d, detail_width)))
            .unwrap_or_else(|| "error".to_string())
    } else if file.last_event_type == "deleted" {
        "deleted".to_string()
    } else {
        format!("indexed ×{}", file.times_indexed)
    }
}

fn rag_file_entry_lines(
    file: &crate::db::project::RagPerFileStatus,
    name_width: usize,
    detail_width: usize,
) -> Vec<Line<'static>> {
    let (icon, icon_color) = rag_file_icon_and_color(&file.last_event_type);
    let filename = std::path::Path::new(&file.file_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file.file_path.clone());
    let name_trunc = truncate_str(&filename, name_width);
    let detail = rag_file_detail(file, detail_width);

    vec![
        Line::from(vec![
            Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
            Span::styled(
                name_trunc,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("   ", Style::default().fg(DIM)),
            Span::styled(detail, Style::default().fg(DIM)),
        ]),
    ]
}

fn rag_file_status_lines(
    files: &[crate::db::project::RagPerFileStatus],
    area: Rect,
) -> Vec<Line<'static>> {
    let max_rows = (area.height as usize).saturating_sub(8).max(2);
    let name_width = area.width.saturating_sub(4) as usize;
    let detail_width = area.width.saturating_sub(6) as usize;

    let mut lines = Vec::new();
    for file in files.iter().take(max_rows) {
        lines.extend(rag_file_entry_lines(file, name_width, detail_width));
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

fn playground_header_lines(app: &App) -> Vec<Line<'static>> {
    let scope_label = playground_scope_label(app);
    let query = &app.playground_query;

    let mut header = vec![Line::from(vec![
        Span::styled("RAG Playground ", Style::default().fg(DIM)),
        Span::styled(
            format!("({scope_label}) "),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            format!("· {query}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ])];

    if app.playground_search_pending {
        header.push(Line::from(vec![
            Span::styled("  ◉ Searching", Style::default().fg(Color::Yellow)),
            Span::styled(" · press Esc to cancel", Style::default().fg(DIM)),
        ]));
    } else if !app.playground_results.is_empty() {
        header.push(Line::from(vec![
            Span::styled("  ✓ ", Style::default().fg(ACCENT)),
            Span::styled(
                format!("{} results", app.playground_results.len()),
                Style::default().fg(ACCENT),
            ),
        ]));
    }

    header.push(Line::from(Span::styled(
        "Type to search · ↑↓ navigate · Tab toggle scope · Enter focus · Ctrl+T transfer · Esc close",
        Style::default().fg(DIM),
    )));
    header.push(Line::from(""));

    header
}

fn playground_empty_state(app: &App) -> Line<'static> {
    let message = if app.playground_query.trim().is_empty() {
        "Start typing to search indexed chunks."
    } else if app.playground_search_pending {
        "◉ Searching..."
    } else {
        "No matching chunks."
    };
    let color = if app.playground_search_pending {
        Color::Yellow
    } else {
        DIM
    };
    Line::from(Span::styled(message, Style::default().fg(color)))
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
    let mut lines = playground_header_lines(app);

    if app.playground_results.is_empty() {
        lines.push(playground_empty_state(app));
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
    } else {
        lines.push(Line::from(Span::styled(
            format!("  {} results", total),
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

    let percent = ((start.saturating_add(visible_lines)).min(total_lines) * 100)
        .checked_div(total_lines)
        .unwrap_or(100)
        .min(100);
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
        found.map_or(BORDER_COLOR, |session| session.accent(app))
    } else {
        BORDER_COLOR
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
        app.last_panel_x = inner.x;
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
        None,
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
    let (pty_area, input_area) = split_warp_areas(area, &warp_input_text(app, terminal_idx));

    if let Some(snap) = snap {
        render_snapshot(frame, pty_area, snap, app, false, false, None);
    }

    if focused && matches!(app.focus, Focus::Agent) {
        draw_warp_input_box(frame, input_area, app, terminal_idx);
    }

    if focused {
        app.last_panel_inner = (pty_area.width, pty_area.height);
        app.last_panel_x = pty_area.x;
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
            SessionRef::Terminal(idx)
                if app.terminal_agents[idx].warp_mode
                    && !app.terminal_agents[idx].should_bypass_warp_input() =>
            {
                Some(idx)
            }
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
    use super::split_warp_areas;
    use super::warp;
    use crate::tui::agent::screen::VtCell;
    use crate::tui::agent::ScreenSnapshot;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    #[test]
    fn copilot_cursor_no_longer_shifted_without_inverse() {
        let snap = ScreenSnapshot {
            cells: vec![(0..8).map(|_| None).collect()],
            cursor_row: 0,
            cursor_col: 5,
            scrolled: false,
        };
        assert_eq!(adjusted_interactive_cursor_col("copilot", &snap), 5);
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
            wide_continuation: false,
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

    #[test]
    fn split_warp_areas_reserves_four_rows_for_empty_input() {
        let area = Rect::new(0, 0, 80, 20);
        let (pty_area, input_area) = split_warp_areas(area, "");
        assert_eq!(input_area.height, 4);
        // 1 row gap + 4 row input box.
        assert_eq!(pty_area.height, 15);
    }

    #[test]
    fn split_warp_areas_leaves_one_row_gap_above_input() {
        let area = Rect::new(0, 0, 80, 20);
        let (pty_area, input_area) = split_warp_areas(area, "hello");
        assert_eq!(input_area.y, pty_area.y + pty_area.height + 1);
    }

    #[test]
    fn warp_input_height_short_text_stays_at_base() {
        // 100 chars at 40 cols wraps to 3 lines, which fits within the
        // base box without growing it.
        let text = "a".repeat(100);
        assert_eq!(warp::input_height(&text, 40), 4);
    }

    #[test]
    fn warp_input_height_long_text_grows() {
        // 200 chars at 40 cols wraps to 5 lines: 2 lines beyond the
        // 3-line base capacity, so the box grows from 4 to 6 rows.
        let text = "a".repeat(200);
        let height = warp::input_height(&text, 40);
        assert!((5..=6).contains(&height), "height was {height}");
    }

    #[test]
    fn warp_input_height_explicit_newlines_grow_and_cap_at_max() {
        let text = "a\nb\nc\nd\ne\nf\ng"; // 7 lines
        assert_eq!(warp::input_height(text, 40), 8);
    }

    #[test]
    fn warp_input_height_empty_is_base() {
        assert_eq!(warp::input_height("", 40), 4);
    }
}
