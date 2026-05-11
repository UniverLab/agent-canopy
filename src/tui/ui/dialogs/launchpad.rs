use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::dialog::LaunchpadChoice;
use crate::tui::app::types::App;

use super::{centered_rect, truncate_str, ACCENT, BG_SELECTED, DIM};

fn mission_view_window(text: &str, cursor_byte: usize, max_cols: usize) -> (String, usize) {
    let max_cols = max_cols.max(1);
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let cursor_char = text[..cursor_byte.min(text.len())]
        .chars()
        .count()
        .min(total);

    if total <= max_cols {
        return (text.to_string(), cursor_char);
    }

    let start = cursor_char.saturating_sub(max_cols.saturating_sub(1));
    let end = (start + max_cols).min(total);
    let display: String = chars[start..end].iter().collect();
    let cursor_col = cursor_char
        .saturating_sub(start)
        .min(max_cols.saturating_sub(1));
    (display, cursor_col)
}

pub fn draw_launchpad_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = &app.launchpad_dialog else {
        return;
    };

    let area = centered_rect(70, 14, frame.area());
    frame.render_widget(Clear, area);

    let title = format!(
        " New Session: {} ",
        truncate_str(&super::super::last_two_segments(&dialog.workdir), 40)
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    frame.render_widget(block, area);

    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );

    let mut lines: Vec<Line> = Vec::new();
    let mut mission_row: Option<u16> = None;
    if let Some(previous) = &dialog.previous {
        lines.push(Line::from(Span::styled(
            "Previous mission (incomplete):",
            Style::default().fg(DIM),
        )));
        lines.push(Line::from(format!(
            "  {}",
            truncate_str(&previous.mission, 80)
        )));
        if let Some(summary) = &previous.summary {
            lines.push(Line::from(Span::styled(
                format!("Previous summary: {}", truncate_str(summary, 78)),
                Style::default().fg(DIM),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No previous mission found for this workspace.",
            Style::default().fg(DIM),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from("What do you want to do today?"));

    let continue_style = if dialog.selected == LaunchpadChoice::ContinuePrevious {
        Style::default().bg(BG_SELECTED).fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    let new_style = if dialog.selected == LaunchpadChoice::NewMission {
        Style::default().bg(BG_SELECTED).fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };

    lines.push(Line::from(Span::styled(
        if dialog.previous.is_some() {
            "  [ Continue previous mission ]"
        } else {
            "  [ Continue previous mission ] (disabled)"
        },
        continue_style,
    )));
    lines.push(Line::from(Span::styled("  [ New mission ]", new_style)));

    if dialog.selected == LaunchpadChoice::NewMission {
        lines.push(Line::from(""));
        mission_row = Some(lines.len() as u16);
        let mission_value_width =
            inner.width.saturating_sub("Mission: ".len() as u16).max(1) as usize;
        let (mission_text, _) =
            mission_view_window(&dialog.new_mission, dialog.cursor, mission_value_width);
        lines.push(Line::from(Span::styled(
            format!("Mission: {mission_text}"),
            Style::default().fg(ratatui::style::Color::White),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled(
            "Enter",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" confirm  ", Style::default().fg(DIM)),
        Span::styled(
            "Tab",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" toggle  ", Style::default().fg(DIM)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel", Style::default().fg(DIM)),
    ]));

    frame.render_widget(Paragraph::new(lines).block(Block::default()), inner);

    if dialog.selected == LaunchpadChoice::NewMission {
        let mission_prefix = "Mission: ";
        let mission_value_width = inner
            .width
            .saturating_sub(mission_prefix.len() as u16)
            .max(1) as usize;
        let (_, cursor_col) =
            mission_view_window(&dialog.new_mission, dialog.cursor, mission_value_width);
        let mission_row = mission_row.unwrap_or(0);
        let cursor_x = inner
            .x
            .saturating_add(mission_prefix.len() as u16)
            .saturating_add(cursor_col as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        let cursor_y = inner.y.saturating_add(mission_row);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}
