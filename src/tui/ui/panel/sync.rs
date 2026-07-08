use chrono::{Local, TimeZone};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::domain::sync::{MessageKind, MissionImpact, SyncMessage, WorkspaceStatus};
use crate::tui::app::types::SyncPanelState;
use crate::tui::ui::{last_two_segments, ACCENT, BORDER_COLOR, DIM, ERROR_COLOR, STATUS_OK};

pub(crate) fn draw_activity_panel(
    frame: &mut Frame,
    area: Rect,
    state: &SyncPanelState,
    scroll_offset: u16,
) {
    let block = Block::default()
        .title(
            Line::from(Span::styled(" activity ", Style::default().fg(DIM)))
                .alignment(ratatui::layout::Alignment::Right),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER_COLOR));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_sync_section(frame, inner, state, scroll_offset);
}

fn draw_sync_section(frame: &mut Frame, area: Rect, state: &SyncPanelState, scroll_offset: u16) {
    let w = area.width as usize;
    let vibe_fg = vibe_color(state.vibe);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("vibe ", Style::default().fg(DIM)),
            Span::styled(
                state.vibe.as_str(),
                Style::default().fg(vibe_fg).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("participants ", Style::default().fg(DIM)),
            Span::styled(
                state.participant_count.to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("workdir ", Style::default().fg(DIM)),
            Span::styled(
                last_two_segments(&state.workdir),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "missions",
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        )),
    ];

    if state.active_intents.is_empty() {
        lines.push(Line::from(Span::styled("  none", Style::default().fg(DIM))));
    } else {
        for intent in &state.active_intents {
            // Card header: agent · impact · status
            lines.push(Line::from(vec![
                Span::styled("┌ ", Style::default().fg(intent_color(intent.impact))),
                Span::styled(
                    &intent.agent_name,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" [{}]", intent.impact.as_str()),
                    Style::default().fg(intent_color(intent.impact)),
                ),
            ]));
            // Mission text — wrap manually
            for chunk in wrap_text(&intent.mission, w.saturating_sub(2)) {
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(intent_color(intent.impact))),
                    Span::styled(chunk, Style::default().fg(Color::White)),
                ]));
            }
            // Description — wrap
            if !intent.description.is_empty() {
                for chunk in wrap_text(&intent.description, w.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(intent_color(intent.impact))),
                        Span::styled(chunk, Style::default().fg(DIM)),
                    ]));
                }
            }
            lines.push(Line::from(Span::styled(
                "└─",
                Style::default().fg(intent_color(intent.impact)),
            )));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "messages",
        Style::default().fg(DIM).add_modifier(Modifier::BOLD),
    )));

    let recent_msgs = recent_messages_for_display(&state.recent_messages);
    if state.active_intents.is_empty() && recent_msgs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  nothing to show",
            Style::default().fg(DIM),
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
        let color = kind_color(message.kind);
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
                Style::default().fg(DIM),
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

fn intent_color(impact: MissionImpact) -> Color {
    match impact {
        MissionImpact::Low => ACCENT,
        MissionImpact::High => Color::Yellow,
        MissionImpact::Breaking => ERROR_COLOR,
    }
}

fn kind_color(kind: MessageKind) -> Color {
    match kind {
        MessageKind::Intent => ACCENT,
        MessageKind::Status => Color::Yellow,
        MessageKind::Query => Color::Cyan,
        MessageKind::Answer => STATUS_OK,
        MessageKind::Info => DIM,
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
