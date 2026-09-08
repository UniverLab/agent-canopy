use chrono::{Local, TimeZone};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::domain::loops::LoopSpecStatus;
use crate::domain::sync::{MessageKind, MissionImpact, SyncMessage, WorkspaceStatus};
use crate::tui::app::types::{App, PanelFace, SyncPanelState};
use crate::tui::ui::theme::Theme;
use crate::tui::ui::{last_two_segments, truncate_str, ERROR_COLOR, STATUS_OK};

/// CT1 multi-face right panel: one bordered panel, three faces. The title
/// names the face; when the panel switched on its own the badge names the
/// reason, so a face never appears unexplained.
pub(crate) fn draw_panel_face(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    let face = app.panel_face;
    let badge = app.panel_face_badge();
    let mut title = format!(" {} ", face.label());
    if let Some(reason) = badge {
        title = format!("{title}· {reason} ");
    }
    let block = Block::default()
        .title(
            Line::from(Span::styled(title, Style::default().fg(theme.dim_text)))
                .alignment(ratatui::layout::Alignment::Right),
        )
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(theme.border_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match face {
        PanelFace::Activity => {
            if let Some(state) = app.activity_panel_state() {
                draw_sync_section(frame, inner, &state, app.sync_scroll_offset, theme);
            } else {
                draw_panel_placeholder(frame, inner, "no activity", theme);
            }
        }
        PanelFace::Knowledge => draw_knowledge_face(frame, inner, app, theme),
        PanelFace::Loop => draw_loop_face(frame, inner, app, theme),
    }
}

fn draw_panel_placeholder(frame: &mut Frame, area: Rect, text: &str, theme: &Theme) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(theme.dim_text),
        ))),
        area,
    );
}

/// Knowledge face: the activity section moved under it, then the
/// project-relations graph (CB16's scrollable renderer, reused — not
/// redrawn), then the knowledge/backlog list for the selected project.
fn draw_knowledge_face(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    let graph_rows = knowledge_graph_height(app, area.height);
    if area.height < 8 || graph_rows == 0 {
        // Too short to split honestly: activity keeps the whole panel.
        if let Some(state) = app.selected_activity_state() {
            draw_sync_section(frame, area, &state, app.sync_scroll_offset, theme);
        } else {
            draw_panel_placeholder(frame, area, "no activity", theme);
        }
        return;
    }

    let [activity_area, graph_area, list_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(graph_rows),
        Constraint::Min(knowledge_list_min_height(app)),
    ])
    .areas(area);

    if let Some(state) = app.selected_activity_state() {
        draw_sync_section(frame, activity_area, &state, app.sync_scroll_offset, theme);
    } else {
        draw_panel_placeholder(frame, activity_area, "no activity", theme);
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "project graph",
            Style::default()
                .fg(theme.dim_text)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect::new(graph_area.x, graph_area.y, graph_area.width, 1),
    );
    let graph_inner = Rect::new(
        graph_area.x,
        graph_area.y + 1,
        graph_area.width,
        graph_area.height.saturating_sub(1),
    );
    crate::tui::ui::sidebar::draw_project_graph(frame, graph_inner, app, theme);

    draw_knowledge_list(frame, list_area, app, theme);
}

/// Rows the graph section needs: one header line plus one row per edge,
/// capped so the activity section above never starves.
fn knowledge_graph_height(app: &App, panel_height: u16) -> u16 {
    if panel_height < 8 {
        return 0;
    }
    let edges = app.project_graph_edges.len();
    let rows = if edges == 0 { 1 } else { edges.min(5) as u16 };
    (rows + 1).min(panel_height / 3).max(2)
}

/// Minimum rows for the knowledge/backlog list: header plus the selected
/// row with one line of context on each side when space allows (CB15's
/// height floor — the section holding the cursor keeps its rows).
fn knowledge_list_min_height(app: &App) -> u16 {
    if app.project_knowledge.is_empty() && app.backlog_specs.is_empty() {
        2
    } else {
        4
    }
}

fn draw_knowledge_list(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "knowledge",
            Style::default()
                .fg(theme.dim_text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " ({}) · backlog ({})",
                app.project_knowledge.len(),
                app.backlog_specs.len()
            ),
            Style::default().fg(theme.dim_text),
        ),
    ])];

    if app.project_knowledge.is_empty() {
        lines.push(Line::from(Span::styled(
            "  none yet",
            Style::default().fg(theme.dim_text),
        )));
    } else {
        let total = app.project_knowledge.len();
        let visible = (area.height as usize).saturating_sub(1).max(1);
        let start = crate::tui::selection::clamp_scroll(app.selected_knowledge, 0, total, visible);
        for (idx, node) in app
            .project_knowledge
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
        {
            let selected = idx == app.selected_knowledge;
            let marker = if selected { "›" } else { " " };
            let kind_color = if node.kind == "fact" {
                Color::Cyan
            } else {
                Color::Magenta
            };
            lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(theme.header_color)),
                Span::raw(" "),
                Span::styled(format!("[{}] ", node.kind), Style::default().fg(kind_color)),
                Span::styled(
                    truncate_str(&node.title, area.width.saturating_sub(10) as usize),
                    if selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    },
                ),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Loop face (read-only first step): the running loop's name, status and
/// CT5's spec strip — same spec-queue data and same done/current/pending
/// markers the Automation view renders, without the live node output and
/// without taking any keys.
fn draw_loop_face(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(target) = app
        .selected_loop_id
        .as_ref()
        .and_then(|id| app.loops.iter().find(|lp| &lp.id == id))
        .or_else(|| {
            app.loops
                .iter()
                .find(|lp| lp.status == crate::domain::loops::LoopStatus::Running)
        })
        .or_else(|| app.loops.first())
    else {
        draw_panel_placeholder(frame, area, "no loops yet", theme);
        return;
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                truncate_str(&target.name, area.width.saturating_sub(12) as usize),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", target.status.as_str()),
                Style::default().fg(loop_status_color(target.status)),
            ),
        ]),
        Line::from(Span::styled(
            truncate_str(
                &format!("workdir {}", last_two_segments(&target.workdir)),
                area.width as usize,
            ),
            Style::default().fg(theme.dim_text),
        )),
        Line::from(""),
    ];

    let strip_known = app
        .loop_live_state
        .as_ref()
        .is_some_and(|live| live.loop_id == target.id);
    if strip_known {
        let live = app.loop_live_state.as_ref().expect("checked above");
        lines.push(Line::from(Span::styled(
            format!("specs {}/{}", live.done_count, live.total_count),
            Style::default().fg(theme.dim_text),
        )));
        for entry in &live.spec_queue {
            let current = live.current_spec_id.as_deref() == Some(entry.spec_id.as_str());
            lines.push(Line::from(vec![
                Span::styled(
                    spec_strip_marker(entry.status, current),
                    Style::default().fg(theme.header_color),
                ),
                Span::raw(" "),
                Span::styled(
                    truncate_str(&entry.spec_name, area.width.saturating_sub(4) as usize),
                    Style::default().fg(Color::White),
                ),
            ]));
        }
    } else if let Some(details) = app.loop_details.as_ref().filter(|d| d.lp.id == target.id) {
        lines.push(Line::from(Span::styled(
            format!("specs ({})", details.specs.len()),
            Style::default().fg(theme.dim_text),
        )));
        for spec in &details.specs {
            lines.push(Line::from(vec![
                Span::styled(
                    spec_strip_marker(spec.spec.status, false),
                    Style::default().fg(theme.header_color),
                ),
                Span::raw(" "),
                Span::styled(
                    truncate_str(&spec.spec.name, area.width.saturating_sub(4) as usize),
                    Style::default().fg(Color::White),
                ),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "open Automation → Loops for specs",
            Style::default().fg(theme.dim_text),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// CT5 spec-strip marker, shared convention: done / current / failed /
/// pending. Pure so it is testable without constructing a full `App`.
fn spec_strip_marker(status: LoopSpecStatus, current: bool) -> &'static str {
    if current {
        return "▶";
    }
    match status {
        LoopSpecStatus::Completed => "●",
        LoopSpecStatus::Running => "▶",
        LoopSpecStatus::Failed => "✗",
        LoopSpecStatus::Skipped => "○",
        LoopSpecStatus::Pending => "○",
        _ => "○",
    }
}

fn loop_status_color(status: crate::domain::loops::LoopStatus) -> Color {
    match status {
        crate::domain::loops::LoopStatus::Running => STATUS_OK,
        crate::domain::loops::LoopStatus::Failed => ERROR_COLOR,
        crate::domain::loops::LoopStatus::Pausing => Color::Yellow,
        crate::domain::loops::LoopStatus::Paused => Color::Yellow,
        _ => Color::White,
    }
}

fn draw_sync_section(
    frame: &mut Frame,
    area: Rect,
    state: &SyncPanelState,
    scroll_offset: u16,
    theme: &Theme,
) {
    let w = area.width as usize;
    let vibe_fg = vibe_color(state.vibe);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("vibe ", Style::default().fg(theme.dim_text)),
            Span::styled(
                state.vibe.as_str(),
                Style::default().fg(vibe_fg).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("participants ", Style::default().fg(theme.dim_text)),
            Span::styled(
                state.participant_count.to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("workdir ", Style::default().fg(theme.dim_text)),
            Span::styled(
                last_two_segments(&state.workdir),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "missions",
            Style::default()
                .fg(theme.dim_text)
                .add_modifier(Modifier::BOLD),
        )),
    ];

    if state.active_intents.is_empty() {
        lines.push(Line::from(Span::styled(
            "  none",
            Style::default().fg(theme.dim_text),
        )));
    } else {
        for intent in &state.active_intents {
            // Card header: agent · impact · status
            lines.push(Line::from(vec![
                Span::styled(
                    "┌ ",
                    Style::default().fg(intent_color(intent.impact, theme)),
                ),
                Span::styled(
                    &intent.agent_name,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" [{}]", intent.impact.as_str()),
                    Style::default().fg(intent_color(intent.impact, theme)),
                ),
            ]));
            // Mission text — wrap manually
            for chunk in wrap_text(&intent.mission, w.saturating_sub(2)) {
                lines.push(Line::from(vec![
                    Span::styled(
                        "│ ",
                        Style::default().fg(intent_color(intent.impact, theme)),
                    ),
                    Span::styled(chunk, Style::default().fg(Color::White)),
                ]));
            }
            // Description — wrap
            if !intent.description.is_empty() {
                for chunk in wrap_text(&intent.description, w.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "│ ",
                            Style::default().fg(intent_color(intent.impact, theme)),
                        ),
                        Span::styled(chunk, Style::default().fg(theme.dim_text)),
                    ]));
                }
            }
            lines.push(Line::from(Span::styled(
                "└─",
                Style::default().fg(intent_color(intent.impact, theme)),
            )));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "messages",
        Style::default()
            .fg(theme.dim_text)
            .add_modifier(Modifier::BOLD),
    )));

    let recent_msgs = recent_messages_for_display(&state.recent_messages);
    if state.active_intents.is_empty() && recent_msgs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  nothing to show",
            Style::default().fg(theme.dim_text),
        )));
    }
    for message in recent_msgs {
        let icon = match message.kind {
            MessageKind::Intent => "◉",
            MessageKind::Status => "≈",
            MessageKind::Query => "?",
            MessageKind::Answer => "↳",
            MessageKind::Info => "·",
        };
        let color = kind_color(message.kind, theme);
        // Card header: icon · session_name · client
        lines.push(Line::from(vec![
            Span::styled(format!("┌{icon} "), Style::default().fg(color)),
            Span::styled(
                &message.agent_name,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(color)),
            Span::styled(
                format_timestamp(message.created_at),
                Style::default().fg(theme.dim_text),
            ),
        ]));
        // Message body — wrap
        for chunk in wrap_text(&message.message, w.saturating_sub(2)) {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(color)),
                Span::styled(chunk, Style::default().fg(Color::White)),
            ]));
        }
        lines.push(Line::from(Span::styled("└─", Style::default().fg(color))));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default())
            .scroll((scroll_offset, 0)),
        area,
    );
}

/// Split `text` into chunks of at most `max_width` chars, breaking on whitespace.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= max_width {
            current.push(' ');
            current.push_str(word);
        } else {
            result.push(current.clone());
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn recent_messages_for_display(messages: &[SyncMessage]) -> Vec<&SyncMessage> {
    messages.iter().rev().collect()
}

fn vibe_color(status: WorkspaceStatus) -> Color {
    match status {
        WorkspaceStatus::Stable => STATUS_OK,
        WorkspaceStatus::Unstable => ERROR_COLOR,
        WorkspaceStatus::Testing => Color::Yellow,
    }
}

fn intent_color(impact: MissionImpact, theme: &Theme) -> Color {
    match impact {
        MissionImpact::Low => theme.header_color,
        MissionImpact::High => Color::Yellow,
        MissionImpact::Breaking => ERROR_COLOR,
    }
}

fn kind_color(kind: MessageKind, theme: &Theme) -> Color {
    match kind {
        MessageKind::Intent => theme.header_color,
        MessageKind::Status => Color::Yellow,
        MessageKind::Query => Color::Cyan,
        MessageKind::Answer => STATUS_OK,
        MessageKind::Info => theme.dim_text,
    }
}

fn format_timestamp(timestamp: i64) -> String {
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(datetime) => datetime.format("%Y-%m-%d %H:%M").to_string(),
        None => timestamp.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::tui::app::types::App;

    fn test_db() -> std::sync::Arc<Database> {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        std::sync::Arc::new(Database::new(&path).expect("create test db"))
    }

    #[test]
    fn spec_strip_marker_distinguishes_done_current_failed_and_pending() {
        assert_eq!(spec_strip_marker(LoopSpecStatus::Completed, false), "●");
        assert_eq!(spec_strip_marker(LoopSpecStatus::Running, false), "▶");
        assert_eq!(spec_strip_marker(LoopSpecStatus::Pending, false), "○");
        assert_eq!(spec_strip_marker(LoopSpecStatus::Failed, false), "✗");
        assert_eq!(spec_strip_marker(LoopSpecStatus::Skipped, false), "○");
        // The current spec reads as running even before its status flips.
        assert_eq!(spec_strip_marker(LoopSpecStatus::Pending, true), "▶");
        assert_eq!(spec_strip_marker(LoopSpecStatus::Completed, true), "▶");
    }

    #[test]
    fn knowledge_graph_height_caps_so_activity_never_starves() {
        let db = test_db();
        let data_dir = tempfile::tempdir().expect("create data dir");
        let mut app = App::new(std::sync::Arc::clone(&db), data_dir.path()).expect("create app");

        // No edges: one placeholder row + header.
        assert_eq!(knowledge_graph_height(&app, 30), 2);
        // Many edges: capped at 5 rows + header.
        app.project_graph_edges = (0..20)
            .map(|i| crate::tui::app::types::ProjectGraphEdge {
                from_name: format!("a{i}"),
                to_name: format!("b{i}"),
                from_hash: format!("ha{i}"),
                to_hash: format!("hb{i}"),
                relation: "relates_to".to_string(),
            })
            .collect();
        assert_eq!(knowledge_graph_height(&app, 30), 6);
        // A short panel refuses to split at all.
        assert_eq!(knowledge_graph_height(&app, 7), 0);
    }

    #[test]
    fn knowledge_list_min_height_keeps_rows_for_the_cursor() {
        let db = test_db();
        let data_dir = tempfile::tempdir().expect("create data dir");
        let app = App::new(std::sync::Arc::clone(&db), data_dir.path()).expect("create app");
        // Empty: header + placeholder only.
        assert_eq!(knowledge_list_min_height(&app), 2);
    }

    #[test]
    fn format_timestamp_returns_non_empty_text() {
        assert!(!format_timestamp(0).is_empty());
    }

    #[test]
    fn recent_messages_for_display_prioritizes_newest_entries() {
        let messages = vec![
            SyncMessage {
                id: 1,
                workdir: "/tmp/project".into(),
                agent_id: "agent-a".into(),
                agent_name: "oak-fern".into(),
                kind: MessageKind::Info,
                message: "older".into(),
                payload: None,
                created_at: 1,
            },
            SyncMessage {
                id: 2,
                workdir: "/tmp/project".into(),
                agent_id: "agent-b".into(),
                agent_name: "moss-hawk".into(),
                kind: MessageKind::Info,
                message: "newer".into(),
                payload: None,
                created_at: 2,
            },
        ];

        let ordered = recent_messages_for_display(&messages);

        assert_eq!(
            ordered
                .into_iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
    }
}
