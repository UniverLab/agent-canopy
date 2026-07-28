use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::types::App;

use super::{centered_rect, truncate_str, ERROR_COLOR};
use crate::tui::ui::theme::Theme;

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

pub fn draw_launchpad_dialog(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(dialog) = &app.launchpad_dialog else {
        return;
    };

    let mission_count = dialog.recent_missions.len();
    let extra_lines = if mission_count > 0 {
        mission_count + 1
    } else {
        0
    };
    let dialog_height = (12 + extra_lines as u16).min(24);
    let area = centered_rect(70, dialog_height, frame.area());
    frame.render_widget(Clear, area);

    let title = format!(
        " New Session: {} ",
        truncate_str(&super::super::last_two_segments(&dialog.workdir), 40)
    );
    let block = Block::default()
        .title(title)
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(theme.header_color))
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

    let is_new_selected = dialog.is_new_mission_selected();
    let new_style = if is_new_selected {
        Style::default()
            .bg(theme.selected_bg)
            .fg(theme.header_color)
    } else {
        Style::default().fg(theme.dim_text)
    };
    let marker = if is_new_selected { ">" } else { " " };
    lines.push(Line::from(Span::styled(
        format!("  {marker} [1] New mission"),
        new_style,
    )));

    if is_new_selected {
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
        if let Some(message) = dialog.validation_message() {
            let style = if dialog.submit_blocked {
                Style::default().fg(ERROR_COLOR)
            } else {
                Style::default().fg(theme.dim_text)
            };
            lines.push(Line::from(Span::styled(format!("  {message}"), style)));
        }
    }

    if dialog.recent_missions.is_empty() {
        lines.push(Line::from(Span::styled(
            "No previous missions found for this workspace.",
            Style::default().fg(theme.dim_text),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Recent missions:",
            Style::default().fg(theme.dim_text),
        )));
        for (i, mission) in dialog.recent_missions.iter().enumerate() {
            let item_index = i + 1; // 0 is "New mission"
            let is_selected = dialog.selected_index == item_index;
            let style = if is_selected {
                Style::default()
                    .bg(theme.selected_bg)
                    .fg(theme.header_color)
            } else {
                Style::default().fg(theme.dim_text)
            };
            let marker = if is_selected { ">" } else { " " };
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} [{}] {}",
                    marker,
                    item_index + 1,
                    truncate_str(&mission.mission, 72)
                ),
                style,
            )));
        }
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
        Span::styled(
            if dialog.can_confirm_selection() {
                " confirm  "
            } else {
                " confirm (disabled)  "
            },
            Style::default().fg(theme.dim_text),
        ),
        Span::styled(
            "Up/Down",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" navigate  ", Style::default().fg(theme.dim_text)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel", Style::default().fg(theme.dim_text)),
    ]));

    frame.render_widget(Paragraph::new(lines).block(Block::default()), inner);

    if is_new_selected {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mission_view_window_short_text() {
        let (text, cursor) = mission_view_window("hello", 0, 20);
        assert_eq!(text, "hello");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn mission_view_window_exact_width() {
        let (text, cursor) = mission_view_window("hello", 0, 5);
        assert_eq!(text, "hello");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn mission_view_window_long_text_cursor_start() {
        let (text, cursor) = mission_view_window("hello world this is long", 0, 10);
        assert_eq!(text.len(), 10);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn mission_view_window_long_text_cursor_end() {
        let text = "hello world this is long";
        let (display, cursor) = mission_view_window(text, text.len(), 10);
        assert!(display.chars().count() <= 10);
        assert!(cursor <= 10);
    }

    #[test]
    fn mission_view_window_cursor_in_middle() {
        let text = "abcdef ghijkl mnopqr";
        let (display, cursor) = mission_view_window(text, 10, 8);
        assert_eq!(display.len(), 8);
        assert!(cursor <= 8);
    }

    #[test]
    fn mission_view_window_max_cols_zero_becomes_one() {
        let (text, _) = mission_view_window("hello", 0, 0);
        assert!(text.len() <= 1);
    }

    #[test]
    fn mission_view_window_unicode() {
        let text = "café résumé";
        let (display, cursor) = mission_view_window(text, 0, 5);
        assert!(!display.is_empty());
        assert!(cursor <= display.chars().count());
    }
}
