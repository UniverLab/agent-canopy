use crate::tui::app::types::{App, Focus};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

pub fn draw_log_text(frame: &mut Frame, area: Rect, inner: Rect, app: &App) {
    let title = app.selected_id();
    let title_suffix = match app.focus {
        Focus::Agent => " (Esc → back)",
        Focus::Preview => " (Enter → focus)",
        Focus::RagTransfer => " (Esc → cancel)",
        _ => "",
    };
    let title_block = Block::default()
        .title(format!(" {title}{title_suffix} "))
        .borders(Borders::NONE);
    frame.render_widget(title_block, area);

    let line_count = app.log_content.lines().count() as u16;
    let max_scroll = line_count.saturating_sub(inner.height);
    let scroll = app.log_scroll.min(max_scroll);

    let paragraph = Paragraph::new(app.log_content.as_str())
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, inner);

    if line_count > inner.height {
        let mut scrollbar_state =
            ScrollbarState::new(line_count as usize).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::default().orientation(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}
