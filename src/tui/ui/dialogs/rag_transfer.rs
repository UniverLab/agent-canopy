use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, draw_dialog_left_wave, truncate_str};
use crate::tui::app::types::App;
use crate::tui::ui::theme::Theme;

pub fn draw_rag_transfer_modal(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(modal) = &app.rag_transfer_modal else {
        return;
    };

    let agents = &app.interactive_agents;
    let card_h = 3u16;
    let visible_cards = agents.len().min(5) as u16;
    let list_h = if agents.is_empty() {
        1
    } else {
        visible_cards * card_h + visible_cards.saturating_sub(1)
    };
    let preview_lines = modal.context_payload.lines().count().min(4) as u16;
    let height = 7 + preview_lines + list_h;
    let area = centered_rect(70, height, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Transfer RAG Result ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.header_color))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_dialog_left_wave(frame, area, app.animation_tick.into());

    let mut lines = vec![
        Line::from(vec![
            Span::styled("  Query: ", Style::default().fg(theme.dim_text)),
            Span::styled(
                truncate_str(&modal.query, inner.width.saturating_sub(10) as usize),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Selected chunk preview:",
            Style::default().fg(theme.dim_text),
        )),
    ];

    for line in modal.context_payload.lines().skip(5).take(4) {
        lines.push(Line::from(Span::styled(
            format!(
                "  {}",
                truncate_str(line, inner.width.saturating_sub(4) as usize)
            ),
            Style::default().fg(Color::Rgb(170, 200, 170)),
        )));
    }

    lines.push(Line::from(""));

    // Full-width trailing filler so a selected row reads as a card spanning the
    // whole dialog (like the sidebar), not just the background behind the text.
    let row_fill =
        |bg: Color| Span::styled(" ".repeat(inner.width as usize), Style::default().bg(bg));

    if agents.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No interactive agents running.",
            Style::default().fg(theme.dim_text),
        )));
    } else {
        for (i, agent) in agents.iter().enumerate() {
            if i > 0 {
                lines.push(Line::from(""));
            }
            let is_sel = i == modal.picker_selected;
            let accent = agent.accent_color;
            // Subtle selection (matches the sidebar): dark-gray background with
            // the agent name in its accent color, instead of a full accent fill.
            let bg = if is_sel {
                theme.selected_bg
            } else {
                Color::Rgb(15, 25, 15)
            };
            let fg = if is_sel { accent } else { Color::White };
            let cursor = if is_sel { "›" } else { " " };
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", cursor), Style::default().fg(accent).bg(bg)),
                Span::styled(
                    &agent.name,
                    Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                ),
                row_fill(bg),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default().bg(bg)),
                Span::styled(
                    format!("pty · {}", agent.cli.as_str()),
                    Style::default().fg(theme.dim_text).bg(bg),
                ),
                row_fill(bg),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default().bg(bg)),
                Span::styled(
                    truncate_path(&agent.working_dir, inner.width.saturating_sub(6) as usize),
                    Style::default().fg(Color::Cyan).bg(bg),
                ),
                row_fill(bg),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑↓ navigate · Enter open prompt builder · Esc cancel",
        Style::default().fg(theme.dim_text),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn truncate_path(path: &str, max_chars: usize) -> String {
    if path.len() <= max_chars {
        return path.to_string();
    }
    let trimmed = &path[path.len() - max_chars.saturating_sub(1)..];
    let start = trimmed.find('/').map(|p| p + 1).unwrap_or(0);
    format!("…/{}", &trimmed[start..])
}
