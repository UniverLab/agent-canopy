//! Renderers for the loop run-time controls' two overlays: the autorun
//! scheduling input and the last action's result banner (see
//! `app::dialog::loop_control`).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::centered_rect;
use crate::tui::app::dialog::{LoopAutorunDialog, LoopAutorunMode};
use crate::tui::app::types::App;
use crate::tui::ui::theme::Theme;

/// The autorun-scheduling dialog (`a` on a focused loop): [`LoopAutorunMode::Picker`]
/// (the default) reuses the same inline date-time picker as the prompt
/// builder's scheduled-send control, shown alongside the local timezone and
/// the resulting UTC instant so the conversion is visible before
/// submission; Tab switches to [`LoopAutorunMode::QuotaMessage`], a
/// free-text field sent verbatim to the daemon — empty cancels any pending
/// autorun.
pub fn draw_loop_autorun_dialog(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(dialog) = &app.loop_autorun_dialog else {
        return;
    };

    let area = centered_rect(60, 11, frame.area());
    frame.render_widget(Clear, area);

    let title = format!(" Autorun: {} ", dialog.loop_name);
    let block = Block::default()
        .title(title)
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(theme.header_color))
        .style(Style::default().bg(theme.dialog_bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();
    match dialog.mode {
        LoopAutorunMode::Picker => {
            let offset = chrono::Local::now().format("%:z").to_string();
            lines.push(Line::from(Span::styled(
                format!("Pick a local time — your timezone is UTC{offset}"),
                Style::default().fg(theme.dim_text),
            )));
            lines.push(Line::from(""));
            lines.push(picker_line(dialog, theme));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("→ ", Style::default().fg(theme.dim_text)),
                Span::styled(
                    format!(
                        "{} UTC",
                        dialog.picker_resulting_utc().format("%Y-%m-%d %H:%M:%S")
                    ),
                    Style::default().fg(Color::White),
                ),
            ]));
        }
        LoopAutorunMode::QuotaMessage => {
            lines.push(Line::from(Span::styled(
                "Quota-reset text for the engine to parse (empty = cancel pending)",
                Style::default().fg(theme.dim_text),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("▸ ", Style::default().fg(theme.header_color)),
                Span::styled(
                    dialog.quota_input.as_str(),
                    Style::default().fg(Color::White),
                ),
                Span::styled("▏", Style::default().fg(theme.header_color)),
            ]));
        }
    }

    if let Some(error) = &dialog.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.as_str(),
            Style::default().fg(Color::Red),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Tab: switch mode  Enter: schedule  Esc: close",
        Style::default().fg(theme.dim_text),
    )));

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y, inner.width, inner.height),
    );
}

/// Render the picker's year/month/day/hour/minute fields with the currently
/// focused one highlighted, mirroring the prompt builder's inline send-at
/// picker rendering.
fn picker_line(dialog: &LoopAutorunDialog, theme: &Theme) -> Line<'static> {
    use chrono::{Datelike, Timelike};
    let v = dialog.picker.value;
    let fields = [
        format!("{:04}", v.year()),
        format!("{:02}", v.month()),
        format!("{:02}", v.day()),
        format!("{:02}", v.hour()),
        format!("{:02}", v.minute()),
    ];
    let separators = ["-", "-", " ", ":", ""];
    let mut spans = vec![Span::styled("▸ ", Style::default().fg(theme.header_color))];
    for (i, field) in fields.iter().enumerate() {
        let style = if i == dialog.picker.field {
            Style::default().fg(theme.dialog_bg).bg(theme.header_color)
        } else {
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(field.clone(), style));
        spans.push(Span::raw(separators[i]));
    }
    Line::from(spans)
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
        .style(Style::default().bg(theme.dialog_bg));
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
    use crate::tui::app::dialog::LoopActionMessage;
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

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn autorun_dialog_picker_mode_renders_fields_and_resulting_utc() {
        let (mut app, _dir) = test_app();
        let dialog = LoopAutorunDialog::new("lp1".to_string(), "Nightly review".to_string(), None);
        app.loop_autorun_dialog = Some(dialog);

        let theme = Theme::classic();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_loop_autorun_dialog(frame, &app, &theme))
            .unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Nightly review"), "{text}");
        assert!(text.contains("timezone"), "{text}");
        assert!(text.contains("UTC"), "{text}");
        assert!(text.contains("schedule"), "{text}");
    }

    #[test]
    fn autorun_dialog_quota_message_mode_renders_typed_text() {
        let (mut app, _dir) = test_app();
        let mut dialog =
            LoopAutorunDialog::new("lp1".to_string(), "Nightly review".to_string(), None);
        dialog.mode = LoopAutorunMode::QuotaMessage;
        dialog.quota_input = "resets 1pm".to_string();
        app.loop_autorun_dialog = Some(dialog);

        let theme = Theme::classic();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_loop_autorun_dialog(frame, &app, &theme))
            .unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Nightly review"), "{text}");
        assert!(text.contains("resets 1pm"), "{text}");
        assert!(text.contains("schedule"), "{text}");
    }

    #[test]
    fn autorun_dialog_shows_inline_error() {
        let (mut app, _dir) = test_app();
        let mut dialog =
            LoopAutorunDialog::new("lp1".to_string(), "Nightly review".to_string(), None);
        dialog.error = Some("picked time is in the past".to_string());
        app.loop_autorun_dialog = Some(dialog);

        let theme = Theme::classic();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_loop_autorun_dialog(frame, &app, &theme))
            .unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("picked time is in the past"), "{text}");
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
