use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, ACCENT, DIM};
use crate::tui::app::types::App;

pub fn draw_workflow_editor_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = &app.workflow_editor_dialog else {
        return;
    };

    let has_error = dialog.parse_error.is_some();
    let height = frame.area().height.saturating_sub(4).clamp(10, 24);
    let area = centered_rect(70, height, frame.area());
    frame.render_widget(Clear, area);

    let border_color = if has_error { Color::Red } else { ACCENT };

    let block = Block::default()
        .title(dialog.title.as_str())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header = vec![
        Line::from(vec![
            Span::styled("Node: ", Style::default().fg(DIM)),
            Span::styled(dialog.node_name.as_str(), Style::default().fg(Color::White)),
        ]),
        Line::from(Span::styled(dialog.help.as_str(), Style::default().fg(DIM))),
    ];
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );

    let error_rows: u16 = if has_error { 2 } else { 0 };
    let editor_area = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(2 + error_rows),
    );
    let body_height = editor_area.height.max(1) as usize;

    let lines: Vec<&str> = dialog.buffer.lines().collect();
    let (cursor_line, cursor_col) = cursor_line_col(&dialog.buffer, dialog.cursor);
    let start_line = cursor_line.saturating_sub(body_height.saturating_sub(1));
    let end_line = (start_line + body_height).min(lines.len().max(1));

    let rendered_lines = if lines.is_empty() {
        vec![Line::from("")]
    } else {
        lines[start_line..end_line]
            .iter()
            .map(|line| Line::from((*line).to_string()))
            .collect::<Vec<_>>()
    };
    frame.render_widget(Paragraph::new(rendered_lines), editor_area);

    if let Some(err) = &dialog.parse_error {
        let error_y = inner.y + 2 + editor_area.height;
        let err_text = vec![
            Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(Color::Red),
            )),
            Line::from(Span::styled(err.as_str(), Style::default().fg(Color::Red))),
        ];
        frame.render_widget(
            Paragraph::new(err_text),
            Rect::new(inner.x, error_y, inner.width, 2),
        );
    }

    let cursor_y = editor_area
        .y
        .saturating_add(cursor_line.saturating_sub(start_line) as u16)
        .min(editor_area.y + editor_area.height.saturating_sub(1));
    let cursor_x = editor_area
        .x
        .saturating_add(cursor_col as u16)
        .min(editor_area.x + editor_area.width.saturating_sub(1));
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn cursor_line_col(text: &str, cursor: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for (index, ch) in text.chars().enumerate() {
        if index == cursor {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}
