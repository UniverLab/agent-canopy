//! Renderers for the loop run-time controls' two overlays: the autorun
//! scheduling input and the last action's result banner (see
//! `app::dialog::loop_control`).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::centered_rect;
use crate::tui::app::types::App;
use crate::tui::ui::theme::Theme;

/// The autorun-scheduling text input (`a` on a focused loop): one free-text
/// field — empty cancels any pending autorun, an ISO 8601 instant schedules
/// directly, anything else is sent as a raw quota-reset message for the
/// daemon to parse.
pub fn draw_loop_autorun_dialog(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(dialog) = &app.loop_autorun_dialog else {
        return;
    };

    let area = centered_rect(60, 8, frame.area());
    frame.render_widget(Clear, area);

    let title = format!(" Autorun: {} ", dialog.loop_name);
    let block = Block::default()
        .title(title)
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(theme.header_color))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            "ISO instant (2026-07-10T09:00:00Z) or quota-reset text",
            Style::default().fg(theme.dim_text),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("▸ ", Style::default().fg(theme.header_color)),
            Span::styled(dialog.input.as_str(), Style::default().fg(Color::White)),
            Span::styled("▏", Style::default().fg(theme.header_color)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Enter: schedule (empty = cancel pending)  Esc: close",
            Style::default().fg(theme.dim_text),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y, inner.width, inner.height),
    );
}

/// The daemon's verbatim result from the last loop-control action
/// (run/pause/continue/reset/autorun) — success or error, shown until
/// dismissed by `App::dismiss_loop_action_message`'s TTL or superseded by
/// the next dispatch. Never swallowed into a silent no-op.
pub fn draw_loop_action_message(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(message) = &app.loop_action_message else {
        return;
    };

    let color = if message.is_error {
        Color::Red
    } else {
        theme.header_color
    };
    let title = if message.is_error {
        " Loop action failed "
    } else {
        " Loop action "
    };

    let dialog_width = frame.area().width * 50 / 100;
    let inner_width = dialog_width.saturating_sub(2).max(1);
    let chars_per_line = inner_width as usize;
    let needed_lines = message
        .text
        .split('\n')
        .map(|line| (line.len().div_ceil(chars_per_line.max(1))).max(1) as u16)
        .sum::<u16>();
    let height = (needed_lines + 2).min(frame.area().height.saturating_sub(2).max(3));

    let area = centered_rect(50, height, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(
        Paragraph::new(message.text.as_str())
            .style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(if message.is_error {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::tui::app::dialog::{LoopActionMessage, LoopAutorunDialog};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn test_app() -> (App, tempfile::TempDir) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let app = App::new(db, data_dir.path()).unwrap();
        (app, data_dir)
    }

    #[test]
    fn autorun_dialog_renders_input_and_hints() {
        let (mut app, _dir) = test_app();
        let mut dialog = LoopAutorunDialog::new("lp1".to_string(), "Nightly review".to_string());
        dialog.input = "resets 1pm".to_string();
        app.loop_autorun_dialog = Some(dialog);

        let theme = Theme::classic();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_loop_autorun_dialog(frame, &app, &theme))
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("Nightly review"), "{text}");
        assert!(text.contains("resets 1pm"), "{text}");
        assert!(text.contains("schedule"), "{text}");
    }

    #[test]
    fn autorun_dialog_absent_when_not_open() {
        let (app, _dir) = test_app();
        let theme = Theme::classic();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_loop_autorun_dialog(frame, &app, &theme))
            .unwrap();
    }

    #[test]
    fn action_message_shows_daemon_success_text() {
        let (mut app, _dir) = test_app();
        app.loop_action_message = Some(LoopActionMessage {
            is_error: false,
            text: "Loop 'lp1' launched in background.".to_string(),
        });

        let theme = Theme::classic();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_loop_action_message(frame, &app, &theme))
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("launched in background"), "{text}");
    }

    #[test]
    fn action_message_shows_daemon_error_text() {
        let (mut app, _dir) = test_app();
        app.loop_action_message = Some(LoopActionMessage {
            is_error: true,
            text: "Loop 'lp1' is not paused.".to_string(),
        });

        let theme = Theme::classic();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_loop_action_message(frame, &app, &theme))
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("is not paused"), "{text}");
        assert!(text.contains("failed"), "{text}");
    }

    #[test]
    fn action_message_absent_when_none() {
        let (app, _dir) = test_app();
        let theme = Theme::classic();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_loop_action_message(frame, &app, &theme))
            .unwrap();
    }
}
