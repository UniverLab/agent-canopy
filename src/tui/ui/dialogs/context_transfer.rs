use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, draw_dialog_left_wave, truncate_str};
use crate::tui::app::types::App;
use crate::tui::context_transfer::ContextTransferStep;
use crate::tui::ui::theme::Theme;

pub fn draw_context_transfer_modal(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(modal) = &app.context_transfer_modal else {
        return;
    };

    match modal.step {
        ContextTransferStep::Preview => draw_ctx_preview(frame, app, theme),
        ContextTransferStep::AgentPicker => draw_ctx_picker(frame, app, theme),
    }
}
fn draw_ctx_preview(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(modal) = &app.context_transfer_modal else {
        return;
    };

    let preview_lines: Vec<&str> = modal.payload_preview.lines().collect();
    let visible_preview = preview_lines.len().min(8) as u16;
    let height = 12 + visible_preview;
    let area = centered_rect(70, height, frame.area());
    frame.render_widget(Clear, area);

    let (src_id, accent) = if modal.source_is_terminal {
        app.terminal_agents
            .get(modal.source_agent_idx)
            .map(|a| (a.name.as_str(), a.accent_color))
            .unwrap_or(("?", theme.header_color))
    } else {
        app.interactive_agents
            .get(modal.source_agent_idx)
            .map(|a| (a.name.as_str(), a.accent_color))
            .unwrap_or(("?", theme.header_color))
    };

    let src_type = if modal.source_is_terminal {
        "terminal"
    } else {
        "agent"
    };
    let block = Block::default()
        .title(format!(" Context Transfer — from: {src_id} "))
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(theme.dialog_bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_dialog_left_wave(frame, area, app.animation_tick.into());

    let active_style = Style::default()
        .fg(Color::Black)
        .bg(accent)
        .add_modifier(Modifier::BOLD);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  From {src_type}: "),
                Style::default().fg(theme.dim_text),
            ),
            Span::styled(format!(" ◀ {} ▶ ", modal.n_units), active_style),
            Span::styled(
                format!("  ({})", modal.unit_label()),
                Style::default().fg(theme.dim_text),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("  Capture: {}", modal.unit_help()),
            Style::default().fg(theme.dim_text),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Preview:",
            Style::default().fg(theme.dim_text),
        )),
    ];

    for line in preview_lines.iter().take(8) {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate_str(line, 60)),
            Style::default().fg(Color::Rgb(170, 200, 170)),
        )));
    }
    if preview_lines.len() > 8 {
        lines.push(Line::from(Span::styled(
            format!("  … {} more lines", preview_lines.len() - 8),
            Style::default().fg(theme.dim_text),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ←→: adjust range · Enter: pick destination · Esc: cancel",
        Style::default().fg(theme.dim_text),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}
fn draw_ctx_picker(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(modal) = &app.context_transfer_modal else {
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
    let height = 4 + list_h + 2;
    let area = centered_rect(66, height, frame.area());
    frame.render_widget(Clear, area);

    let src_accent = app
        .interactive_agents
        .get(modal.source_agent_idx)
        .map(|a| a.accent_color)
        .unwrap_or(theme.header_color);

    let block = Block::default()
        .title(" Select Destination Agent ")
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(src_accent))
        .style(Style::default().bg(theme.dialog_bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_dialog_left_wave(frame, area, app.animation_tick.into());

    let mut lines = vec![Line::from("")];

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
            let is_src = i == modal.source_agent_idx;

            let bar_color = if is_src {
                theme.dim_text
            } else {
                agent.accent_color
            };
            let accent = agent.accent_color;
            // Subtle selection (matches the sidebar): dark-gray background with
            // the agent name in its accent color, instead of a full accent fill.
            let id_color = if is_sel {
                accent
            } else if is_src {
                theme.dim_text
            } else {
                Color::White
            };
            let bg = if is_sel {
                theme.selected_bg
            } else {
                theme.dialog_bg
            };

            let cursor = if is_sel { "›" } else { " " };
            let src_tag = if is_src { "  (source)" } else { "" };

            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", cursor),
                    Style::default().fg(bar_color).bg(bg),
                ),
                Span::styled(
                    format!("{}{}", agent.name, src_tag),
                    Style::default()
                        .fg(id_color)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
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

            let dir = truncate_path(&agent.working_dir, inner.width.saturating_sub(6) as usize);
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default().bg(bg)),
                Span::styled(dir, Style::default().fg(Color::Cyan).bg(bg)),
                row_fill(bg),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑↓ navigate · Enter transfer · Esc back",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_path_short_enough() {
        assert_eq!(truncate_path("/a/b/c", 20), "/a/b/c");
    }

    #[test]
    fn truncate_path_exact_length() {
        assert_eq!(truncate_path("/a/b/c", 6), "/a/b/c");
    }

    #[test]
    fn truncate_path_too_long() {
        let result = truncate_path("/home/user/project/file.txt", 15);
        assert!(result.starts_with("…/"));
        assert!(result.len() <= 17); // "…/" + max_chars
    }

    #[test]
    fn truncate_path_single_slash() {
        let result = truncate_path("/very/long/path/to/something", 5);
        assert!(result.starts_with("…/"));
    }
}
