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

    if let Some((content_length, position, viewport)) =
        log_scrollbar_geometry(line_count, inner.height, app.log_scroll)
    {
        let mut scrollbar_state = ScrollbarState::new(content_length)
            .position(position)
            .viewport_content_length(viewport);
        frame.render_stateful_widget(
            Scrollbar::default().orientation(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}

/// Returns (content_length, position, viewport_content_length) for the log
/// scrollbar, or None when all lines fit and no scrollbar is needed.
pub(super) fn log_scrollbar_geometry(
    line_count: u16,
    inner_height: u16,
    scroll: u16,
) -> Option<(usize, usize, usize)> {
    if line_count <= inner_height {
        return None;
    }
    let max_scroll = line_count.saturating_sub(inner_height);
    let position = scroll.min(max_scroll);
    Some((
        line_count as usize,
        position as usize,
        inner_height as usize,
    ))
}

#[cfg(test)]
mod tests {
    use super::log_scrollbar_geometry;

    #[test]
    fn fits_within_viewport_returns_none() {
        assert_eq!(log_scrollbar_geometry(10, 20, 0), None);
    }

    #[test]
    fn exactly_fills_viewport_returns_none() {
        assert_eq!(log_scrollbar_geometry(20, 20, 0), None);
    }

    #[test]
    fn scrolled_to_max_reaches_bottom_of_thumb_range() {
        let line_count = 50;
        let inner_height = 10;
        let max_scroll = line_count - inner_height;
        let (content_length, position, viewport) =
            log_scrollbar_geometry(line_count, inner_height, max_scroll).unwrap();
        assert_eq!(position, content_length - viewport);
    }

    #[test]
    fn scroll_beyond_max_is_clamped_to_bottom() {
        let line_count = 50;
        let inner_height = 10;
        let (content_length, position, viewport) =
            log_scrollbar_geometry(line_count, inner_height, u16::MAX).unwrap();
        assert_eq!(position, content_length - viewport);
    }

    #[test]
    fn scroll_at_zero_yields_position_zero() {
        let (_, position, _) = log_scrollbar_geometry(50, 10, 0).unwrap();
        assert_eq!(position, 0);
    }
}
