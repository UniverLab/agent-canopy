//! Renderer for [`LoopFormDialog`] — create/edit a loop's metadata and
//! trigger (T9). Mirrors the loop node editor's visual style.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::centered_rect;
use crate::tui::app::dialog::loop_form::{
    LoopFormDialog, LoopTriggerChoice, FIELD_DESCRIPTION, FIELD_NAME, FIELD_TRIGGER_KIND,
    FIELD_TRIGGER_VALUE, FIELD_WORKDIR,
};
use crate::tui::app::types::App;
use crate::tui::ui::theme::Theme;

pub fn draw_loop_form_dialog(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(dialog) = &app.loop_form_dialog else {
        return;
    };

    let has_error = dialog.error.is_some();
    let height = 12 + if has_error { 2 } else { 0 };
    let area = centered_rect(64, height, frame.area());
    frame.render_widget(Clear, area);

    let title = if dialog.is_edit_mode() {
        " Edit Loop "
    } else {
        " New Loop "
    };
    let border_color = if has_error {
        Color::Red
    } else {
        theme.header_color
    };
    let block = Block::default()
        .title(title)
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.dialog_bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![
        text_field_line(dialog, FIELD_NAME, "Name", &dialog.name, theme),
        text_field_line(
            dialog,
            FIELD_DESCRIPTION,
            "Description",
            &dialog.description,
            theme,
        ),
        text_field_line(dialog, FIELD_WORKDIR, "Workdir", &dialog.workdir, theme),
        trigger_kind_line(dialog, theme),
    ];
    match dialog.trigger_choice {
        LoopTriggerChoice::Manual => {}
        LoopTriggerChoice::Cron => {
            lines.push(text_field_line(
                dialog,
                FIELD_TRIGGER_VALUE,
                "Cron expr",
                &dialog.cron_expr,
                theme,
            ));
        }
        LoopTriggerChoice::Watch => {
            lines.push(text_field_line(
                dialog,
                FIELD_TRIGGER_VALUE,
                "Watch path",
                &dialog.watch_path,
                theme,
            ));
            lines.push(Line::from(vec![
                Span::styled("  Events      ", Style::default().fg(theme.dim_text)),
                Span::styled(
                    dialog.watch_events.join(", "),
                    Style::default().fg(theme.dim_text),
                ),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Tab/↑↓ field · ←→ trigger · Enter save · Esc cancel",
        Style::default().fg(theme.dim_text),
    )));
    if let Some(err) = &dialog.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err.as_str(),
            Style::default().fg(Color::Red),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, inner.y + 1, inner.width, inner.height),
    );
}

fn text_field_line<'a>(
    dialog: &LoopFormDialog,
    field: usize,
    label: &'a str,
    value: &'a str,
    theme: &Theme,
) -> Line<'a> {
    let focused = dialog.field == field;
    let marker = if focused { "▸ " } else { "  " };
    let label_style = if focused {
        Style::default()
            .fg(theme.header_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.dim_text)
    };
    let value_style = if focused {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
    };
    let cursor = if focused { "▏" } else { "" };
    Line::from(vec![
        Span::styled(format!("{marker}{label:<12}"), label_style),
        Span::styled(value.to_string(), value_style),
        Span::styled(cursor, Style::default().fg(theme.header_color)),
    ])
}

fn trigger_kind_line<'a>(dialog: &'a LoopFormDialog, theme: &Theme) -> Line<'a> {
    let focused = dialog.field == FIELD_TRIGGER_KIND;
    let marker = if focused { "▸ " } else { "  " };
    let label_style = if focused {
        Style::default()
            .fg(theme.header_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.dim_text)
    };
    let spans = vec![
        Span::styled(format!("{marker}{:<12}", "Trigger"), label_style),
        Span::styled(
            if focused { "◂ " } else { "  " },
            Style::default().fg(theme.header_color),
        ),
        Span::styled(
            dialog.trigger_choice.label(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if focused { " ▸" } else { "  " },
            Style::default().fg(theme.header_color),
        ),
    ];
    Line::from(spans)
}
