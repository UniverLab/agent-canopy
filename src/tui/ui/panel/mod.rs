//! Right panel rendering — PTY output, brain automaton, banner, background_agent/watcher details, log.

use chrono::{Local, TimeZone};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use super::theme::Theme;
use super::{truncate_str, truncate_str_keep_tail, STATUS_RUNNING};
use crate::tui::agent::ScreenSnapshot;
use crate::tui::app::types::{AgentEntry, App, Focus, ProjectTab, SidebarLayer};

pub mod background_agent;
pub mod details;
pub mod home;
pub mod log_fallback;
mod loop_live;
pub mod sync;
pub mod vt100;
pub mod warp;

pub(crate) use background_agent::draw_background_agent_panel;
pub use details::{draw_agent_details, draw_group_details};
pub(crate) use home::draw_brians_brain;
pub use log_fallback::draw_log_text;
use loop_live::draw_loop_live_view;
pub(crate) use sync::draw_panel_face;
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
    theme: &Theme,
) -> Rect {
    let mut block = Block::default()
        .borders(super::borders_for(theme))
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
    let Ok(nodes) = app
        .db
        .search_operational_sessions(&project.path, limit.saturating_mul(4))
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

fn labeled_value_line<'a>(label: &'static str, value: Span<'a>, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(label, Style::default().fg(theme.dim_text)),
        value,
    ])
}

fn selected_row_style(selected: bool, theme: &Theme) -> (Style, &'static str) {
    if selected {
        (Style::default().bg(theme.selected_bg), "›")
    } else {
        (Style::default(), " ")
    }
}

fn selected_agent_accent(app: &App, theme: &Theme) -> Option<Color> {
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
        _ => Some(theme.header_color),
    }
}

/// Resolves (border color, label color) for the log panel given the current
/// focus and the selected agent's accent (if any). Pure so it's testable
/// without constructing a full `App`.
fn panel_focus_colors(focus: Focus, agent_accent: Option<Color>, theme: &Theme) -> (Color, Color) {
    let accent = agent_accent.unwrap_or(theme.border_color);
    match focus {
        // Full-border accent for real focus.
        Focus::Agent => (accent, accent),
        // Preview is quieter: normal border, accent only on the label.
        Focus::Preview => (theme.border_color, accent),
        _ => (theme.border_color, theme.border_color),
    }
}

fn log_panel_border_color(app: &App, theme: &Theme) -> Color {
    panel_focus_colors(app.focus, selected_agent_accent(app, theme), theme).0
}

fn panel_mode_label_color(app: &App, theme: &Theme) -> Color {
    panel_focus_colors(app.focus, selected_agent_accent(app, theme), theme).1
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
        && app.projects.is_empty()
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

fn draw_log_panel_focus(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) -> bool {
    match app.focus {
        Focus::Home => {
            draw_home_panel(frame, area, app);
            true
        }
        Focus::Preview => draw_preview_panel(frame, area, app, theme),
        Focus::Agent if app.sidebar_layer == SidebarLayer::Knowledge => {
            draw_project_tabs_panel(frame, area, app, theme);
            true
        }
        Focus::Agent => draw_agent_panel(frame, area, app, theme),
        Focus::NewAgentDialog => draw_new_agent_dialog_background(frame, area, app),
        Focus::LaunchpadDialog
        | Focus::KnowledgeDialog
        | Focus::ContextTransfer
        | Focus::RagTransfer
        | Focus::PromptTemplateDialog
        | Focus::LoopEditorDialog
        | Focus::LoopFormDialog => false,
        Focus::ProjectRelationDialog => {
            draw_project_preview_card(frame, area, app, theme);
            true
        }
    }
}

fn draw_preview_panel(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) -> bool {
    if app.playground_active {
        draw_playground_panel(frame, area, app, theme);
        return true;
    }

    if app.sidebar_layer == SidebarLayer::Knowledge {
        draw_project_preview_card(frame, area, app, theme);
        return true;
    }

    if app.sidebar_layer == SidebarLayer::Automation
        && app.automation_kind == crate::tui::app::AutomationKind::Loop
    {
        draw_loop_live_view(frame, area, app, theme);
        return true;
    }

    if app.agents_rag_focused && app.rag_info.has_rag_activity() {
        draw_rag_info_overview(frame, area, app, theme);
        return true;
    }

    let Some(selected) = app.selected_agent() else {
        return false;
    };

    draw_selected_preview(frame, area, app, selected, theme)
}

fn draw_agent_panel(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) -> bool {
    if app.playground_active {
        draw_playground_panel(frame, area, app, theme);
        return true;
    }

    let Some(selected) = app.selected_agent() else {
        return false;
    };

    match selected {
        AgentEntry::Interactive(idx) => draw_focused_interactive_panel(frame, area, app, *idx),
        AgentEntry::Terminal(idx) => draw_focused_terminal_panel(frame, area, app, *idx),
        AgentEntry::Group(idx) => {
            draw_group_details(frame, area, app, *idx, theme);
            true
        }
        AgentEntry::Agent(agent) => {
            draw_background_agent_panel(frame, area, agent, app, theme);
            true
        }
        AgentEntry::Corrupt(corrupt) => {
            draw_corrupt_agent_panel(frame, area, corrupt);
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

/// Renders a corrupt agent row's detail panel: id and parse error, no
/// attempt to interpret the malformed data.
fn draw_corrupt_agent_panel(
    frame: &mut Frame,
    area: Rect,
    corrupt: &crate::domain::models::CorruptAgent,
) {
    let text = format!(
        "{} [corrupt config]\n\nThis agent's trigger_config failed to parse and has been \
         quarantined (disabled). It was not modified or reinterpreted.\n\nError: {}\n\nPress 'd' \
         to delete this row.",
        corrupt.id, corrupt.error
    );
    let paragraph = ratatui::widgets::Paragraph::new(text)
        .style(ratatui::style::Style::default().fg(ratatui::style::Color::Red))
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(paragraph, area);
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

fn draw_selected_preview(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    selected: &AgentEntry,
    theme: &Theme,
) -> bool {
    match selected {
        AgentEntry::Agent(agent) => {
            draw_agent_details(frame, area, agent, app, theme);
            true
        }
        AgentEntry::Corrupt(corrupt) => {
            draw_corrupt_agent_panel(frame, area, corrupt);
            true
        }
        AgentEntry::Interactive(idx) => draw_interactive_preview(frame, area, app, *idx),
        AgentEntry::Terminal(idx) => draw_terminal_preview(frame, area, app, *idx),
        AgentEntry::Group(idx) => {
            draw_group_details(frame, area, app, *idx, theme);
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

pub(super) fn draw_log_panel(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let border_color = log_panel_border_color(app, theme);
    let label_color = panel_mode_label_color(app, theme);
    let title = panel_mode_label(app).map(|label| {
        Span::styled(
            label,
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::BOLD),
        )
    });
    let inner = render_panel_block(frame, area, border_color, title, theme);
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

    if draw_log_panel_focus(frame, inner, app, theme) {
        return;
    }

    draw_log_text(frame, area, inner, app);
}

fn format_intent_lines(
    state: &crate::tui::app::types::SyncPanelState,
    _area_width: u16,
    theme: &Theme,
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
                Style::default().fg(theme.dim_text),
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

fn format_recent_session_lines(sessions: &[(String, String)], theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if sessions.is_empty() {
        lines.push(Line::from("  none"));
        return lines;
    }

    for (title, summary) in sessions {
        lines.push(Line::from(format!("  - {}", truncate_str(title, 92))));
        lines.push(Line::from(Span::styled(
            format!("    {}", summary),
            Style::default().fg(theme.dim_text),
        )));
    }
    lines
}

fn build_project_overview_lines<'a>(
    project: &'a crate::domain::project::Project,
    project_activity: Option<&crate::tui::app::types::SyncPanelState>,
    recent_sessions: &[(String, String)],
    theme: &Theme,
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
            Span::styled("Project ", Style::default().fg(theme.dim_text)),
            Span::styled(
                &project.name,
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format!("workdir_hash: {}", project.hash)),
        Line::from(format!("path: {}", project.path)),
        Line::from(format!("indexed_at: {}", indexed)),
        Line::from(format!("created_at: {}", created)),
        Line::from(format!("tags: {}", tags)),
        Line::from(""),
        Line::from(Span::styled(
            "description",
            Style::default().fg(theme.dim_text),
        )),
        Line::from(description),
        Line::from(""),
        Line::from(Span::styled(
            "workspace context",
            Style::default().fg(theme.dim_text),
        )),
    ];

    if let Some(state) = project_activity {
        lines.push(Line::from(format!(
            "participants: {}  vibe: {}",
            state.participant_count,
            state.vibe.as_str()
        )));
        lines.extend(format_intent_lines(state, 0, theme));
        lines.extend(format_recent_activity_lines(state));
    } else {
        lines.push(Line::from("participants: 0  vibe: stable"));
        lines.push(Line::from("active missions: none"));
        lines.push(Line::from("recent activity: none"));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "recent sessions",
        Style::default().fg(theme.dim_text),
    )));
    lines.extend(format_recent_session_lines(recent_sessions, theme));

    lines
}

fn draw_project_overview(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(project) = app.selected_project() else {
        frame.render_widget(
            Paragraph::new("No registered projects").style(Style::default().fg(theme.dim_text)),
            area,
        );
        return;
    };

    let project_activity = app.activity_panel_state_for_workdir(&project.path);
    let recent_sessions = recent_project_session_summaries(app, project, 3);
    let lines =
        build_project_overview_lines(project, project_activity.as_ref(), &recent_sessions, theme);

    render_wrapped_paragraph(frame, area, lines);
}

/// Knowledge layer's Preview (project highlighted, not entered): a cheap
/// summary card — pending backlog count, knowledge entry count, last
/// activity, and a badge if a loop is running against this project's
/// workdir (functional requirement 3). Reads `App::selected_project_preview`,
/// a cache refreshed on the normal tick cadence — never recomputed here.
fn draw_project_preview_card(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if app.playground_active {
        draw_playground_panel(frame, area, app, theme);
        return;
    }

    let Some(project) = app.selected_project() else {
        frame.render_widget(
            Paragraph::new("No registered projects").style(Style::default().fg(theme.dim_text)),
            area,
        );
        return;
    };

    let summary = app.selected_project_preview();
    let running_badge = if summary.is_some_and(|s| s.loop_running) {
        Span::styled("  ● loop running", Style::default().fg(STATUS_RUNNING))
    } else {
        Span::raw("")
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Project ", Style::default().fg(theme.dim_text)),
            Span::styled(
                &project.name,
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
            ),
            running_badge,
        ]),
        Line::from(format!("path: {}", project.path)),
        Line::from(""),
    ];

    match summary {
        Some(summary) => {
            lines.push(Line::from(vec![
                Span::styled("Backlog: ", Style::default().fg(theme.dim_text)),
                Span::styled(
                    summary.pending_backlog.to_string(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("   Knowledge: ", Style::default().fg(theme.dim_text)),
                Span::styled(
                    summary.knowledge_entries.to_string(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            let last_activity = summary
                .last_activity
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                .map(|dt| crate::tui::app::utils::relative_time(&dt))
                .unwrap_or_else(|| "no activity yet".to_string());
            lines.push(Line::from(vec![
                Span::styled("Last activity: ", Style::default().fg(theme.dim_text)),
                Span::styled(last_activity, Style::default().fg(Color::White)),
            ]));
        }
        None => lines.push(Line::from(Span::styled(
            "Summary loading…",
            Style::default().fg(theme.dim_text),
        ))),
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter → Overview | Backlog | Knowledge | History",
        Style::default().fg(theme.header_color),
    )));

    render_wrapped_paragraph(frame, area, lines);
}

/// Knowledge layer's Focus (project entered, `Enter`): the tab bar —
/// Overview | Backlog | Knowledge | History — plus the active tab's lazily
/// loaded content (functional requirement 4). Populates
/// `project_tab_click_map`/`project_tab_row_click_map` for mouse
/// hit-testing, reusing the same click-map pattern as the sidebar.
fn draw_project_tabs_panel(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    app.project_tab_click_map.clear();
    app.project_tab_row_click_map.clear();

    if area.height == 0 {
        return;
    }
    let Some(project) = app.selected_project().cloned() else {
        frame.render_widget(
            Paragraph::new("No registered projects").style(Style::default().fg(theme.dim_text)),
            area,
        );
        return;
    };
    let Some(active_tab) = app.project_focus else {
        return;
    };

    let [tab_bar_area, content_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    draw_project_tab_bar(frame, tab_bar_area, app, &project.name, active_tab, theme);

    match active_tab {
        ProjectTab::Overview => draw_project_overview(frame, content_area, app, theme),
        ProjectTab::Backlog => draw_backlog_overview(frame, content_area, app, theme),
        ProjectTab::Knowledge => draw_knowledge_overview(frame, content_area, app, theme),
        ProjectTab::History => draw_project_history_tab(frame, content_area, app, theme),
    }
}

fn draw_project_tab_bar(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    project_name: &str,
    active_tab: ProjectTab,
    theme: &Theme,
) {
    let mut spans = vec![
        Span::styled(" ", Style::default()),
        Span::styled(
            project_name,
            Style::default()
                .fg(theme.header_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    let mut x = area.x
        + spans
            .iter()
            .map(|s| s.content.chars().count() as u16)
            .sum::<u16>();

    for tab in ProjectTab::ALL {
        let label = format!(" {} ", tab.label());
        let start = x;
        let selected = tab == active_tab;
        spans.push(Span::styled(
            label.clone(),
            if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.header_color)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.dim_text)
            },
        ));
        let width = label.chars().count() as u16;
        app.project_tab_click_map.push((tab, start, start + width));
        x += width;
        spans.push(Span::raw(" "));
        x += 1;
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Persisted per-project History tab: finished loops + past sessions for
/// this project's workdir, read straight from the DB (functional
/// requirement 5) — see `App::selected_project_history_entries`.
fn draw_project_history_tab(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    let selected = app.selected_project_history;
    let entries = app.selected_project_history_entries().to_vec();
    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new("No history yet for this project.")
                .style(Style::default().fg(theme.dim_text)),
            area,
        );
        return;
    }

    let visible_count = (area.height as usize).min(entries.len());
    for (y, (idx, entry)) in (area.y..).zip(entries.iter().take(visible_count).enumerate()) {
        let is_selected = idx == selected;
        let (style, marker) = selected_row_style(is_selected, theme);
        let kind_label = match entry.kind {
            crate::db::project::ProjectHistoryKind::Loop => "loop",
            crate::db::project::ProjectHistoryKind::InteractiveSession => "session",
            crate::db::project::ProjectHistoryKind::TerminalSession => "terminal",
        };
        let when = chrono::DateTime::from_timestamp(entry.at, 0)
            .map(|dt| crate::tui::app::utils::relative_time(&dt))
            .unwrap_or_default();
        let line = Line::from(vec![
            Span::styled(marker, style.fg(theme.header_color)),
            Span::raw(" "),
            Span::styled(format!("[{kind_label}] "), style.fg(theme.dim_text)),
            Span::styled(
                truncate_str(&entry.name, area.width.saturating_sub(20) as usize),
                style.fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} · {}", entry.status, when),
                style.fg(theme.dim_text),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(area.x, y, area.width, 1));
        app.project_tab_row_click_map.push((idx, y, y + 1));
    }
}

/// Read-only preview of the selected backlog spec — name + description,
/// same "focus already previews it" convention as `draw_loop_overview` and
/// `draw_knowledge_overview` (no dedicated confirm step needed).
fn draw_backlog_overview(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if app.backlog_specs.is_empty() {
        frame.render_widget(
            Paragraph::new("No backlog specs yet").style(Style::default().fg(theme.dim_text)),
            area,
        );
        return;
    }

    let Some(spec) = app.backlog_specs.get(app.selected_backlog) else {
        frame.render_widget(
            Paragraph::new("No backlog spec selected").style(Style::default().fg(theme.dim_text)),
            area,
        );
        return;
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Backlog ", Style::default().fg(theme.dim_text)),
            Span::styled(
                spec.name.as_str(),
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];

    match spec.description.as_deref() {
        Some(description) if !description.is_empty() => {
            for line in description.lines() {
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(Color::White),
                )));
            }
        }
        _ => lines.push(Line::from(Span::styled(
            "(no description)",
            Style::default().fg(theme.dim_text),
        ))),
    }

    render_wrapped_paragraph(frame, area, lines);
}

fn draw_knowledge_overview(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if app.project_knowledge.is_empty() {
        frame.render_widget(
            Paragraph::new("No knowledge yet. Agents can add facts/patterns.")
                .style(Style::default().fg(theme.dim_text)),
            area,
        );
        return;
    }

    let Some(node) = app.project_knowledge.get(app.selected_knowledge) else {
        frame.render_widget(
            Paragraph::new("No knowledge selected").style(Style::default().fg(theme.dim_text)),
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
            Span::styled("Knowledge ", Style::default().fg(theme.dim_text)),
            Span::styled(
                node.title.as_str(),
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
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

fn rag_status(app: &App, theme: &Theme) -> (&'static str, Color) {
    use crate::rag::status::{compute_rag_status, RagModelStatus};

    match compute_rag_status(
        &app.rag_embeddings_model,
        app.rag_paused,
        app.rag_model_loaded,
        app.rag_info.processing_items,
        app.rag_acquisition_state.clone(),
    ) {
        RagModelStatus::Unavailable(_) => ("✗ unavailable", Color::Red),
        RagModelStatus::DownloadFailed(_) => ("✗ download failed", Color::Red),
        RagModelStatus::Downloading { .. } => ("⬇ downloading", Color::Yellow),
        RagModelStatus::Preparing { .. } => ("⚙ preparing", Color::Yellow),
        RagModelStatus::Paused => ("⏸ paused", Color::Yellow),
        RagModelStatus::Ready if app.rag_info.processing_items > 0 => ("◉ indexing", Color::Yellow),
        RagModelStatus::Ready => ("● ready", theme.header_color),
        RagModelStatus::Sleeping => ("○ sleeping", theme.dim_text),
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
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Chunks: ", Style::default().fg(theme.dim_text)),
            Span::styled(
                app.rag_info.total_chunks.to_string(),
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Files: ", Style::default().fg(theme.dim_text)),
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
            theme,
        ),
    ];
    if !queue_text.is_empty() {
        lines.push(labeled_value_line(
            "Queue:  ",
            Span::styled(queue_text, Style::default().fg(Color::White)),
            theme,
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Enter to open the global RAG playground.",
        Style::default().fg(theme.header_color),
    )));
    lines
}

fn draw_rag_info_overview(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let (status_text, status_color) = rag_status(app, theme);
    let mut lines = rag_summary_lines(app, status_text, status_color, rag_queue_text(app), theme);

    match crate::rag::status::compute_rag_status(
        &app.rag_embeddings_model,
        app.rag_paused,
        app.rag_model_loaded,
        app.rag_info.processing_items,
        app.rag_acquisition_state.clone(),
    ) {
        crate::rag::status::RagModelStatus::Unavailable(reason) => {
            lines.push(Line::from(Span::styled(
                format!("  {reason}"),
                Style::default().fg(Color::Red),
            )));
        }
        crate::rag::status::RagModelStatus::Downloading { started_at } => {
            lines.push(Line::from(Span::styled(
                format!(
                    "  downloading model ({}s so far)",
                    crate::rag::status::elapsed_secs(started_at)
                ),
                Style::default().fg(Color::Yellow),
            )));
        }
        crate::rag::status::RagModelStatus::Preparing { started_at } => {
            lines.push(Line::from(Span::styled(
                format!(
                    "  preparing model ({}s so far)",
                    crate::rag::status::elapsed_secs(started_at)
                ),
                Style::default().fg(Color::Yellow),
            )));
        }
        crate::rag::status::RagModelStatus::DownloadFailed(reason) => {
            lines.push(Line::from(Span::styled(
                format!("  download failed: {reason} — retry: canopy rag model retry"),
                Style::default().fg(Color::Red),
            )));
        }
        _ => {}
    }

    if !app.rag_file_status.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Recent files  ", Style::default().fg(theme.dim_text)),
            Span::styled(
                format!("({}) ", app.rag_file_status.len()),
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.extend(rag_file_status_lines(&app.rag_file_status, area, theme));
    } else if !app.global_rag_queue.is_empty() {
        lines.extend(rag_queue_lines(
            &app.global_rag_queue,
            app.selected_rag_queue,
            theme,
        ));
    }

    render_wrapped_paragraph(frame, area, lines);
}

fn rag_file_icon_and_color(event_type: &str, theme: &Theme) -> (&'static str, Color) {
    match event_type {
        "indexed" => ("✓", Color::Green),
        "deleted" => ("○", theme.dim_text),
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
    theme: &Theme,
) -> Vec<Line<'static>> {
    let (icon, icon_color) = rag_file_icon_and_color(&file.last_event_type, theme);
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
            Span::styled("   ", Style::default().fg(theme.dim_text)),
            Span::styled(detail, Style::default().fg(theme.dim_text)),
        ]),
    ]
}

fn rag_file_status_lines(
    files: &[crate::db::project::RagPerFileStatus],
    area: Rect,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let max_rows = (area.height as usize).saturating_sub(8).max(2);
    let name_width = area.width.saturating_sub(4) as usize;
    let detail_width = area.width.saturating_sub(6) as usize;

    let mut lines = Vec::new();
    for file in files.iter().take(max_rows) {
        lines.extend(rag_file_entry_lines(file, name_width, detail_width, theme));
    }

    if max_rows < files.len() {
        let remaining = files.len() - max_rows;
        lines.push(Line::from(Span::styled(
            format!("  … {} more (open playground for full details)", remaining),
            Style::default().fg(theme.dim_text),
        )));
    }
    lines
}

fn rag_queue_lines(
    queue: &[crate::db::project::RagQueueItem],
    selected: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Queue items ", Style::default().fg(theme.dim_text)),
            Span::styled(
                format!("({})", queue.len()),
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    for (idx, item) in queue.iter().enumerate().take(8) {
        let (line_style, marker) = selected_row_style(idx == selected, theme);
        let status_color = if item.status == "processing" {
            Color::Yellow
        } else {
            theme.header_color
        };
        lines.push(Line::from(vec![
            Span::styled(marker, line_style.fg(status_color)),
            Span::raw(" "),
            Span::styled(
                truncate_str(&item.source_path, 40),
                line_style.fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", item.status), line_style.fg(theme.dim_text)),
        ]));
    }

    lines
}

fn draw_playground_panel(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if app.playground_detail_mode {
        draw_playground_detail(frame, area, app, theme);
    } else {
        draw_playground_list(frame, area, app, theme);
    }
}

fn playground_header_lines(app: &App, theme: &Theme) -> Vec<Line<'static>> {
    let scope_label = playground_scope_label(app);
    let query = &app.playground_query;

    let mut header = vec![Line::from(vec![
        Span::styled("RAG Playground ", Style::default().fg(theme.dim_text)),
        Span::styled(
            format!("({scope_label}) "),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            format!("· {query}"),
            Style::default()
                .fg(theme.header_color)
                .add_modifier(Modifier::BOLD),
        ),
    ])];

    if app.playground_search_pending {
        header.push(Line::from(vec![
            Span::styled("  ◉ Searching", Style::default().fg(Color::Yellow)),
            Span::styled(
                " · press Esc to cancel",
                Style::default().fg(theme.dim_text),
            ),
        ]));
    } else if !app.playground_results.is_empty() {
        header.push(Line::from(vec![
            Span::styled("  ✓ ", Style::default().fg(theme.header_color)),
            Span::styled(
                format!("{} results", app.playground_results.len()),
                Style::default().fg(theme.header_color),
            ),
        ]));
    }

    header.push(Line::from(Span::styled(
        "Type to search · ↑↓ navigate · Tab toggle scope · Enter focus · Ctrl+T transfer · Esc close",
        Style::default().fg(theme.dim_text),
    )));
    header.push(Line::from(""));

    header
}

fn playground_empty_state(app: &App, theme: &Theme) -> Line<'static> {
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
        theme.dim_text
    };
    Line::from(Span::styled(message, Style::default().fg(color)))
}

fn visible_playground_window(area: Rect, selected: usize, total: usize) -> (usize, usize) {
    let max_visible = ((area.height.saturating_sub(4)) / 5).max(1) as usize;
    let scroll_start = crate::tui::selection::clamp_scroll(selected, 0, total, max_visible);
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

fn draw_playground_list(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let mut lines = playground_header_lines(app, theme);

    if app.playground_results.is_empty() {
        lines.push(playground_empty_state(app, theme));
        render_wrapped_paragraph(frame, area, lines);
        return;
    }

    let total = app.playground_results.len();
    let (max_visible, scroll_start) =
        visible_playground_window(area, app.playground_selected, total);
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
            theme,
        ));
    }

    if total > max_visible {
        lines.push(Line::from(Span::styled(
            format!("  {}/{} results", app.playground_selected + 1, total),
            Style::default().fg(theme.dim_text),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("  {} results", total),
            Style::default().fg(theme.dim_text),
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
    theme: &Theme,
) -> Vec<Line<'a>> {
    let (style, marker) = selected_row_style(selected, theme);
    let dist = chunk
        .distance
        .map_or("—".to_string(), |d| format!("{d:.3}"));
    let path = format!("{} · {} [dist={}]", project_name, chunk.file_path, dist);
    let mut lines = vec![Line::from(vec![
        Span::styled(marker, style.fg(theme.header_color)),
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
                style.fg(theme.dim_text),
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines
}

fn playground_detail_header(
    chunk: &crate::rag::vector_store::SearchResult,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let dist = chunk
        .distance
        .map_or("—".to_string(), |d| format!("{d:.4}"));
    vec![
        Line::from(vec![
            Span::styled("‹ ", Style::default().fg(theme.header_color)),
            Span::styled(
                format!("{} [dist={}]", chunk.file_path, dist),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "↑↓ scroll · Enter/Ctrl+T transfer · Esc back to list",
            Style::default().fg(theme.dim_text),
        )),
        Line::from(""),
    ]
}

fn detail_progress_line(
    total_lines: usize,
    visible_lines: usize,
    start: usize,
    theme: &Theme,
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
        Style::default().fg(theme.dim_text),
    )))
}

fn draw_playground_detail(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(chunk) = app.playground_results.get(app.playground_selected) else {
        return;
    };

    let mut lines = playground_detail_header(chunk, theme);
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

    if let Some(progress) = detail_progress_line(content_lines.len(), visible_lines, start, theme) {
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
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let found = find_session_by_name(app, session_name);
    let border_color = if focused {
        found.map_or(theme.border_color, |session| session.accent(app))
    } else {
        theme.border_color
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

    let inner = render_panel_block(frame, area, border_color, Some(title), theme);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if focused {
        app.last_panel_inner = (inner.width, inner.height);
        app.last_panel_x = inner.x;
        app.last_panel_y = inner.y;
    }

    let Some(session) = found else {
        render_missing_session(frame, inner, session_name, theme);
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

fn render_missing_session(frame: &mut Frame, area: Rect, session_name: &str, theme: &Theme) {
    let message = Paragraph::new(format!("  Session '{session_name}' not found"))
        .style(Style::default().fg(theme.dim_text));
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
    use super::super::theme::Theme;
    use super::adjusted_interactive_cursor_col;
    use super::draw_backlog_overview;
    use super::panel_focus_colors;
    use super::split_warp_areas;
    use super::warp;
    use super::*;
    use crate::tui::agent::screen::VtCell;
    use crate::tui::agent::ScreenSnapshot;
    use crate::tui::app::types::{App, Focus};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn render_to_text(
        width: u16,
        height: u16,
        draw: impl FnOnce(&mut ratatui::Frame, Rect),
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn backlog_overview_previews_selected_spec_name_and_description_read_only() {
        use crate::db::Database;
        use crate::domain::loops::{LoopSpec, LoopSpecStatus};
        use std::sync::Arc;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        db.insert_loop_spec(&LoopSpec {
            id: "spec-1".to_string(),
            loop_id: None,
            name: "Add retry backoff".to_string(),
            description: Some("## Objective\nRetry requests with backoff.".to_string()),
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();

        let data_dir = tempfile::tempdir().unwrap();
        let app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        assert_eq!(app.backlog_specs.len(), 1, "backlog spec should be loaded");

        let text = render_to_text(50, 10, |frame, area| {
            draw_backlog_overview(frame, area, &app, &Theme::classic());
        });

        assert!(
            text.contains("Add retry backoff"),
            "expected spec name in preview, got:\n{text}"
        );
        assert!(
            text.contains("Retry requests with backoff."),
            "expected spec description in preview, got:\n{text}"
        );
    }

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

    #[test]
    fn focus_agent_draws_full_accent_border() {
        let theme = Theme::classic();
        let accent = Color::Rgb(200, 50, 50);
        let (border, label) = panel_focus_colors(Focus::Agent, Some(accent), &theme);
        assert_eq!(border, accent);
        assert_eq!(label, accent);
    }

    #[test]
    fn focus_preview_keeps_normal_border_and_accents_only_label() {
        let theme = Theme::classic();
        let accent = Color::Rgb(50, 200, 50);
        let (border, label) = panel_focus_colors(Focus::Preview, Some(accent), &theme);
        assert_eq!(border, theme.border_color);
        assert_eq!(label, accent);
    }

    #[test]
    fn other_focus_states_use_normal_border_and_label() {
        let theme = Theme::classic();
        let accent = Color::Rgb(50, 50, 200);
        let (border, label) = panel_focus_colors(Focus::Home, Some(accent), &theme);
        assert_eq!(border, theme.border_color);
        assert_eq!(label, theme.border_color);
    }

    #[test]
    fn missing_accent_falls_back_to_border_color_everywhere() {
        let theme = Theme::classic();
        assert_eq!(
            panel_focus_colors(Focus::Agent, None, &theme),
            (theme.border_color, theme.border_color)
        );
        assert_eq!(
            panel_focus_colors(Focus::Preview, None, &theme),
            (theme.border_color, theme.border_color)
        );
    }

    #[test]
    fn format_unix_timestamp_valid() {
        let ts = 1_700_000_000; // 2023-11-14 22:13:20 UTC
        let result = format_unix_timestamp(ts);
        assert!(result.contains("2023"), "Should contain year: {result}");
    }

    #[test]
    fn format_unix_timestamp_zero() {
        let result = format_unix_timestamp(0);
        // Epoch 0 is 1970-01-01 UTC, displayed as local time
        assert!(!result.is_empty(), "Should produce a string: {result}");
    }

    #[test]
    fn render_wrapped_paragraph_zero_area() {
        let backend = TestBackend::new(10, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 0, 0);
                render_wrapped_paragraph(frame, area, vec![]);
            })
            .unwrap();
    }

    #[test]
    fn render_panel_block_with_title() {
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::classic();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let title = Span::styled(" Test ", Style::default().fg(Color::White));
                let inner = render_panel_block(frame, area, Color::Cyan, Some(title), &theme);
                assert!(inner.width > 0);
                assert!(inner.height > 0);
            })
            .unwrap();
    }

    #[test]
    fn render_panel_block_without_title() {
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::classic();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let inner = render_panel_block(frame, area, Color::Cyan, None, &theme);
                assert!(inner.width > 0);
                assert!(inner.height > 0);
            })
            .unwrap();
    }

    #[test]
    fn render_panel_block_modern_theme_no_borders() {
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::modern();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let inner = render_panel_block(frame, area, Color::Cyan, None, &theme);
                // Modern theme: no borders, so inner == area
                assert_eq!(inner, area);
            })
            .unwrap();
    }

    #[test]
    fn set_cursor_from_snapshot_scrolled_no_cursor() {
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let snap = ScreenSnapshot {
            cells: vec![],
            cursor_row: 5,
            cursor_col: 5,
            scrolled: true,
        };
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 20, 10);
                set_cursor_from_snapshot(frame, area, &snap);
            })
            .unwrap();
        // Scrolled: no cursor set
    }

    #[test]
    fn set_cursor_from_snapshot_zero_area() {
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let snap = ScreenSnapshot {
            cells: vec![],
            cursor_row: 0,
            cursor_col: 0,
            scrolled: false,
        };
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 0, 0);
                set_cursor_from_snapshot(frame, area, &snap);
            })
            .unwrap();
    }

    #[test]
    fn labeled_value_line_renders() {
        let theme = Theme::classic();
        let line = labeled_value_line("Key: ", Span::raw("value"), &theme);
        assert_eq!(line.width(), 10); // "Key: " + "value"
    }

    #[test]
    fn selected_row_style_selected() {
        let theme = Theme::classic();
        let (style, marker) = selected_row_style(true, &theme);
        assert_eq!(style.bg, Some(theme.selected_bg));
        assert_eq!(marker, "›");
    }

    #[test]
    fn selected_row_style_not_selected() {
        let theme = Theme::classic();
        let (style, marker) = selected_row_style(false, &theme);
        assert_eq!(style.bg, None);
        assert_eq!(marker, " ");
    }

    #[test]
    fn panel_mode_label_preview() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.focus = Focus::Preview;
        assert_eq!(panel_mode_label(&app), Some(" Preview "));
    }

    #[test]
    fn panel_mode_label_agent() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.focus = Focus::Agent;
        assert_eq!(panel_mode_label(&app), Some(" Focus "));
    }

    #[test]
    fn panel_mode_label_home() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.focus = Focus::Home;
        assert_eq!(panel_mode_label(&app), None);
    }

    #[test]
    fn show_home_fallback_empty_agents_and_projects() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.focus = Focus::Home;
        assert!(show_home_fallback(&app));
    }

    #[test]
    fn show_home_fallback_not_when_dialog_open() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.focus = Focus::NewAgentDialog;
        assert!(!show_home_fallback(&app));
    }

    #[test]
    fn draw_log_panel_zero_area_no_panic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::classic();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 0, 0);
                draw_log_panel(frame, area, &mut app, &theme);
            })
            .unwrap();
    }

    #[test]
    fn draw_split_panel_zero_area_no_panic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::classic();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 0, 0);
                draw_split_panel(frame, area, &mut app, "test-session", true, &theme);
            })
            .unwrap();
    }

    #[test]
    fn draw_corrupt_agent_panel_renders() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let corrupt = crate::domain::models::CorruptAgent {
            id: "bad-agent".to_string(),
            enabled: false,
            error: "failed to parse JSON".to_string(),
        };
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_corrupt_agent_panel(frame, area, &corrupt);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("bad-agent"), "Should show agent id: {text}");
        assert!(
            text.contains("corrupt config"),
            "Should show corrupt config: {text}"
        );
    }

    #[test]
    fn draw_home_panel_renders() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let app = App::new(db, data_dir.path()).unwrap();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_home_panel(frame, area, &app);
            })
            .unwrap();
    }

    #[test]
    fn rag_file_icon_and_color_indexed() {
        let theme = Theme::classic();
        let (icon, color) = rag_file_icon_and_color("indexed", &theme);
        assert_eq!(icon, "✓");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn rag_file_icon_and_color_deleted() {
        let theme = Theme::classic();
        let (icon, color) = rag_file_icon_and_color("deleted", &theme);
        assert_eq!(icon, "○");
        assert_eq!(color, theme.dim_text);
    }

    #[test]
    fn rag_file_icon_and_color_error() {
        let theme = Theme::classic();
        let (icon, color) = rag_file_icon_and_color("error", &theme);
        assert_eq!(icon, "✗");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn rag_file_detail_error_with_message() {
        use crate::db::project::RagPerFileStatus;
        let file = RagPerFileStatus {
            file_path: "test.rs".to_string(),
            last_event_type: "error".to_string(),
            last_detail: Some("parse failed".to_string()),
            times_indexed: 0,
            last_at: 0,
        };
        let detail = rag_file_detail(&file, 50);
        assert!(detail.starts_with("error:"));
    }

    #[test]
    fn rag_file_detail_error_no_message() {
        use crate::db::project::RagPerFileStatus;
        let file = RagPerFileStatus {
            file_path: "test.rs".to_string(),
            last_event_type: "error".to_string(),
            last_detail: None,
            times_indexed: 0,
            last_at: 0,
        };
        let detail = rag_file_detail(&file, 50);
        assert_eq!(detail, "error");
    }

    #[test]
    fn rag_file_detail_deleted() {
        use crate::db::project::RagPerFileStatus;
        let file = RagPerFileStatus {
            file_path: "test.rs".to_string(),
            last_event_type: "deleted".to_string(),
            last_detail: None,
            times_indexed: 0,
            last_at: 0,
        };
        let detail = rag_file_detail(&file, 50);
        assert_eq!(detail, "deleted");
    }

    #[test]
    fn rag_file_detail_indexed() {
        use crate::db::project::RagPerFileStatus;
        let file = RagPerFileStatus {
            file_path: "test.rs".to_string(),
            last_event_type: "indexed".to_string(),
            last_detail: None,
            times_indexed: 3,
            last_at: 0,
        };
        let detail = rag_file_detail(&file, 50);
        assert_eq!(detail, "indexed ×3");
    }

    #[test]
    fn visible_playground_window_basic() {
        let area = Rect::new(0, 0, 80, 20);
        let (max_visible, scroll_start) = visible_playground_window(area, 0, 20);
        assert!(max_visible > 0);
        assert_eq!(scroll_start, 0);
    }

    #[test]
    fn visible_playground_window_scrolled() {
        let area = Rect::new(0, 0, 80, 20);
        let (max_visible, scroll_start) = visible_playground_window(area, 10, 20);
        assert!(scroll_start > 0 || max_visible >= 10);
    }

    #[test]
    fn playground_scope_label_global() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let app = App::new(db, data_dir.path()).unwrap();
        assert_eq!(playground_scope_label(&app), "Global");
    }

    #[test]
    fn detail_progress_line_all_visible() {
        let theme = Theme::classic();
        let result = detail_progress_line(5, 10, 0, &theme);
        assert!(result.is_none());
    }

    #[test]
    fn detail_progress_line_partial() {
        let theme = Theme::classic();
        let result = detail_progress_line(100, 10, 50, &theme);
        assert!(result.is_some());
    }

    #[test]
    fn rag_status_lines_empty_queue() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.rag_info.queued_items = 0;
        let theme = Theme::classic();
        let lines = rag_summary_lines(&app, "● ready", theme.header_color, String::new(), &theme);
        assert!(!lines.is_empty());
    }

    #[test]
    fn rag_status_lines_with_queue() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let app = App::new(db, data_dir.path()).unwrap();
        let theme = Theme::classic();
        let lines = rag_summary_lines(
            &app,
            "● ready",
            theme.header_color,
            "3 queued".to_string(),
            &theme,
        );
        assert!(!lines.is_empty());
    }

    #[test]
    #[cfg(not(feature = "local-embeddings"))]
    fn rag_status_reports_unavailable_for_local_model_without_feature() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        // Even a "loaded" daemon state must not mask the capability gap.
        app.rag_embeddings_model = "baai/bge-small-en-v1.5".to_string();
        app.rag_model_loaded = true;
        let theme = Theme::classic();
        let (text, color) = rag_status(&app, &theme);
        assert_eq!(text, "✗ unavailable");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn rag_status_reports_downloading_and_not_ready() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.rag_embeddings_model = "text-embedding-3-small".to_string();
        // Even a "loaded" daemon state must not hide an in-progress download.
        app.rag_model_loaded = true;
        app.rag_acquisition_state =
            Some(crate::rag::status::AcquisitionState::Downloading { started_at: 0 });
        let theme = Theme::classic();
        let (text, color) = rag_status(&app, &theme);
        assert_eq!(text, "⬇ downloading");
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn rag_status_reports_preparing() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.rag_embeddings_model = "text-embedding-3-small".to_string();
        app.rag_acquisition_state =
            Some(crate::rag::status::AcquisitionState::Preparing { started_at: 0 });
        let theme = Theme::classic();
        let (text, color) = rag_status(&app, &theme);
        assert_eq!(text, "⚙ preparing");
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn rag_status_reports_download_failed() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(crate::db::Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.rag_embeddings_model = "text-embedding-3-small".to_string();
        app.rag_acquisition_state = Some(crate::rag::status::AcquisitionState::Failed {
            reason: "connection reset".to_string(),
        });
        let theme = Theme::classic();
        let (text, color) = rag_status(&app, &theme);
        assert_eq!(text, "✗ download failed");
        assert_eq!(color, Color::Red);
    }
}
