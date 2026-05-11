use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::domain::sync::{MessageKind, MissionImpact, WorkspaceStatus};
use crate::tui::app::types::SyncPanelState;
use crate::tui::ui::{last_two_segments, ACCENT, DIM, ERROR_COLOR, STATUS_OK};

pub(crate) fn draw_sync_panel(
    frame: &mut Frame,
    area: Rect,
    state: &SyncPanelState,
    scroll_offset: u16,
) {
    let block = Block::default()
        .title(
            Line::from(Span::styled(" sync ", Style::default().fg(DIM)))
                .alignment(ratatui::layout::Alignment::Right),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM));
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
            Span::styled("sessions ", Style::default().fg(DIM)),
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
                if is_redundant_id(&intent.agent_name, &intent.agent_id) {
                    Span::raw("")
                } else {
                    Span::styled(format!(" ({})", intent.agent_id), Style::default().fg(DIM))
                },
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

    for message in &state.recent_messages {
        let icon = match message.kind {
            MessageKind::Intent => "◉",
            MessageKind::Status => "≈",
            MessageKind::Query => "?",
            MessageKind::Answer => "↳",
            MessageKind::Info => "·",
        };
        let color = kind_color(message.kind);
        // Card header: icon agent_name (agent_id)
        lines.push(Line::from(vec![
            Span::styled(format!("┌{icon} "), Style::default().fg(color)),
            Span::styled(
                &message.agent_name,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            if is_redundant_id(&message.agent_name, &message.agent_id) {
                Span::raw("")
            } else {
                Span::styled(format!(" ({})", message.agent_id), Style::default().fg(DIM))
            },
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

/// Returns `true` when `id` is just a slugified form of `name` — e.g.
/// "Copilot CLI" → "copilot-cli" or "copilot_cli". In that case showing
/// `(id)` next to the name adds no information.
fn is_redundant_id(name: &str, id: &str) -> bool {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let norm_id: String = id
        .chars()
        .map(|c| {
            if c == '_' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    slug == norm_id || slug.replace('-', "") == norm_id.replace('-', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_redundant_id_detects_slug() {
        assert!(is_redundant_id("Copilot CLI", "copilot-cli"));
        assert!(is_redundant_id("Copilot CLI", "copilot_cli"));
        assert!(is_redundant_id("My Agent", "my-agent"));
    }

    #[test]
    fn is_redundant_id_keeps_distinct_ids() {
        assert!(!is_redundant_id("Copilot CLI", "agent-42"));
        assert!(!is_redundant_id("Alice", "bob"));
    }
}
