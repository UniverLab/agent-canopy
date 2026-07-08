use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::types::App;
use crate::tui::ui::BORDER_COLOR;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

pub fn draw_knowledge_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = app.knowledge_dialog.as_ref() else {
        return;
    };

    let dialog_area = centered_rect(60, 50, frame.area());

    let block = Block::default()
        .title(if dialog.edit_id.is_some() {
            " Edit Knowledge "
        } else {
            " New Knowledge "
        })
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT));

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(inner);

    draw_field(frame, chunks[0], "Title", &dialog.title, dialog.field == 0);

    draw_field(
        frame,
        chunks[1],
        "Kind",
        dialog.kind_str(),
        dialog.field == 2,
    );

    draw_multiline_field(frame, chunks[2], "Body", &dialog.body, dialog.field == 1);

    let help = Line::from(vec![
        Span::styled(
            "Tab",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" field  "),
        Span::styled(
            "Space",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" toggle kind  "),
        Span::styled(
            "Enter",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" save  "),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" cancel"),
    ]);
    frame.render_widget(Paragraph::new(help), chunks[4]);
}

fn draw_field(frame: &mut Frame, area: Rect, label: &str, value: &str, focused: bool) {
    let border_color = if focused { ACCENT } else { BORDER_COLOR };
    let block = Block::default()
        .title(format!(" {} ", label))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if value.is_empty() && !focused {
        Paragraph::new("...").style(Style::default().fg(DIM))
    } else {
        Paragraph::new(value).style(Style::default().fg(Color::White))
    };

    frame.render_widget(text, inner);
}

fn draw_multiline_field(frame: &mut Frame, area: Rect, label: &str, value: &str, focused: bool) {
    let border_color = if focused { ACCENT } else { BORDER_COLOR };
    let block = Block::default()
        .title(format!(" {} ", label))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if value.is_empty() && !focused {
        Paragraph::new("...")
            .style(Style::default().fg(DIM))
            .wrap(Wrap { trim: false })
    } else {
        Paragraph::new(value)
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false })
    };

    frame.render_widget(text, inner);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}
