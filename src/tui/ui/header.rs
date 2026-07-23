//! Header bar rendering — animated title + daemon status indicator.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::theme::Theme;
use super::ERROR_COLOR;
use crate::shared::banner::BANNER_GRADIENT;
use crate::tui::app::types::App;
use crate::tui::whimsg::TITLE;

/// Return the first `n` chars of `s`, respecting char boundaries.
fn first_n_chars(s: &str, n: usize) -> &str {
    let end = s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len());
    &s[..end]
}

const SPINNER: [&str; 8] = ["⣷", "⣯", "⣟", "⡿", "⢿", "⣻", "⣽", "⣾"];

fn gradient_wave_color(char_idx: usize, shift: usize) -> Color {
    let len = BANNER_GRADIENT.len();
    if len == 0 {
        return Color::White;
    }
    if len == 1 {
        let (r, g, b) = BANNER_GRADIENT[0];
        return Color::Rgb(r, g, b);
    }

    // Mirror cycle to keep a smooth sequence:
    // 0..N-1..1.. and repeat (no hard jumps between dark/light neighbors).
    let cycle_len = len * 2 - 2;
    let pos = (char_idx + shift) % cycle_len;
    let gradient_idx = if pos < len { pos } else { cycle_len - pos };
    let (r, g, b) = BANNER_GRADIENT[gradient_idx];
    Color::Rgb(r, g, b)
}

fn push_animated_gradient_text(spans: &mut Vec<Span>, visible: &str, millis: u128) {
    let shift = ((millis / 90) as usize) % (BANNER_GRADIENT.len() * 2 - 1).max(1);
    for (i, ch) in visible.chars().enumerate() {
        spans.push(Span::styled(
            ch.to_string(),
            Style::default()
                .fg(gradient_wave_color(i, shift))
                .add_modifier(Modifier::BOLD),
        ));
    }
}

pub(super) fn draw_header(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let (status_char, status_color) = if app.daemon_running {
        // Smooth spinner in green, no blink
        let frame_idx = ((millis / 125) % 8) as usize;
        (SPINNER[frame_idx], theme.header_color)
    } else {
        // Blinking █ in red when stopped
        let blink_on = (millis / 500).is_multiple_of(2);
        let color = if blink_on {
            ERROR_COLOR
        } else {
            Color::Rgb(120, 60, 60)
        };
        ("█", color)
    };

    let wf = app.whimsg.tick();
    let mut spans: Vec<Span> = Vec::new();
    // Leading padding so the title/whimsg block isn't flush against the left border
    spans.push(Span::raw(" "));

    if wf.title_visible > 0 {
        // Title partially or fully visible - animated gradient per letter.
        let visible = first_n_chars(TITLE, wf.title_visible);
        push_animated_gradient_text(&mut spans, visible, millis);
        spans.push(Span::raw(" "));
    } else if !wf.kaomoji.is_empty() && wf.text_visible == 0 && wf.text.is_empty() {
        // Kaomoji flash with the same animated gradient style as the title.
        push_animated_gradient_text(&mut spans, &wf.kaomoji, millis);
        spans.push(Span::raw(" "));
    } else if !wf.kaomoji.is_empty() {
        // Kaomoji with animated gradient + message in gray without background.
        let visible_text = first_n_chars(&wf.text, wf.text_visible);
        push_animated_gradient_text(&mut spans, &wf.kaomoji, millis);
        spans.push(Span::raw(" "));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{} ", visible_text),
            Style::default()
                .fg(Color::Rgb(140, 140, 140))
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        // Blank phase — leading space already present
    }

    let left = Paragraph::new(Line::from(spans));
    frame.render_widget(left, area);

    // Daemon status spinner on the right edge
    if area.width > 3 {
        let status = Paragraph::new(Line::from(Span::styled(
            status_char,
            Style::default().fg(status_color),
        )));
        let status_area = Rect::new(area.x + area.width - 2, area.y, 1, 1);
        frame.render_widget(status, status_area);
    }
}
