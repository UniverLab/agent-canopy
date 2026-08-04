use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::centered_rect;
use crate::tui::app::types::{App, LoopEditorDialog, LoopEditorMode, RouterField};
use crate::tui::ui::theme::Theme;

pub fn draw_loop_editor_dialog(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(dialog) = &app.loop_editor_dialog else {
        return;
    };

    let has_error = dialog.parse_error.is_some();
    let height = frame.area().height.saturating_sub(4).clamp(10, 24);
    let area = centered_rect(70, height, frame.area());
    frame.render_widget(Clear, area);

    let border_color = if has_error {
        Color::Red
    } else {
        theme.header_color
    };

    let block = Block::default()
        .title(dialog.title.as_str())
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header = vec![
        Line::from(vec![
            Span::styled("Node: ", Style::default().fg(theme.dim_text)),
            Span::styled(dialog.node_name.as_str(), Style::default().fg(Color::White)),
        ]),
        Line::from(Span::styled(
            dialog.help.as_str(),
            Style::default().fg(theme.dim_text),
        )),
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

    if matches!(dialog.mode, LoopEditorMode::RouterRoutes) {
        draw_router_routes_body(frame, dialog, editor_area, theme);
    } else {
        draw_text_buffer_body(frame, dialog, editor_area);
    }

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
}

fn draw_text_buffer_body(frame: &mut Frame, dialog: &LoopEditorDialog, editor_area: Rect) {
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

/// A router node's structured routes/fallback/wiring form — declares each
/// route's label + description, wires it to a target node, and marks one as
/// the fallback (functional requirement: "declares routes, wires an edge to
/// each, and picks the fallback"). No cursor to place: every field is a
/// short append/pop-at-end edit (see [`LoopEditorDialog::router_push_char`]).
fn draw_router_routes_body(
    frame: &mut Frame,
    dialog: &LoopEditorDialog,
    area: Rect,
    theme: &Theme,
) {
    let fallback_style = if dialog.router_fallback.is_empty() {
        Style::default().fg(theme.dim_text)
    } else {
        Style::default()
            .fg(theme.header_color)
            .add_modifier(Modifier::BOLD)
    };
    let fallback_text = if dialog.router_fallback.is_empty() {
        "(none)".to_string()
    } else {
        dialog.router_fallback.clone()
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Fallback: ", Style::default().fg(theme.dim_text)),
            Span::styled(fallback_text, fallback_style),
        ]),
        Line::from(""),
    ];
    lines.extend(router_route_lines(dialog, theme));

    frame.render_widget(Paragraph::new(lines), area);
}

fn router_route_lines(dialog: &LoopEditorDialog, theme: &Theme) -> Vec<Line<'static>> {
    dialog
        .router_routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let is_focused_route = index == dialog.router_route_index;
            let marker = if is_focused_route { "▸ " } else { "  " };

            let label_text = if route.label.is_empty() {
                "(label)".to_string()
            } else {
                route.label.clone()
            };
            let desc_text = if route.description.is_empty() {
                "(description)".to_string()
            } else {
                route.description.clone()
            };
            let target_text = route
                .target_node_id
                .as_ref()
                .and_then(|id| {
                    dialog
                        .router_targets
                        .iter()
                        .find(|(target_id, _)| target_id == id)
                })
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| "(unwired)".to_string());

            let mut spans = vec![
                Span::raw(marker),
                Span::styled(
                    label_text,
                    field_style(
                        is_focused_route,
                        dialog.router_field == RouterField::Label,
                        theme,
                    ),
                ),
                Span::raw("  —  "),
                Span::styled(
                    desc_text,
                    field_style(
                        is_focused_route,
                        dialog.router_field == RouterField::Description,
                        theme,
                    ),
                ),
                Span::raw("  →  "),
                Span::styled(
                    target_text,
                    field_style(
                        is_focused_route,
                        dialog.router_field == RouterField::Target,
                        theme,
                    ),
                ),
            ];
            if !dialog.router_fallback.is_empty() && dialog.router_fallback == route.label {
                spans.push(Span::styled(
                    "  [fallback]",
                    Style::default()
                        .fg(theme.header_color)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn field_style(is_focused_route: bool, is_focused_field: bool, theme: &Theme) -> Style {
    if is_focused_route && is_focused_field {
        Style::default()
            .fg(theme.header_color)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else if is_focused_route {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(theme.dim_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::tui::app::types::RouterRouteDraft;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn render_to_text(width: u16, height: u16, draw: impl FnOnce(&mut Frame, Rect)) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, area);
            })
            .unwrap();

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

    fn test_app_with_router_dialog() -> App {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.loop_editor_dialog = Some(LoopEditorDialog::new_router_routes(
            "router1".to_string(),
            "Classify".to_string(),
            " Router Routes · Classify ".to_string(),
            "Tab field · Ctrl+S save".to_string(),
            vec![
                RouterRouteDraft {
                    label: "billing".to_string(),
                    description: "billing desc".to_string(),
                    target_node_id: Some("n1".to_string()),
                },
                RouterRouteDraft {
                    label: "technical".to_string(),
                    description: "technical desc".to_string(),
                    target_node_id: None,
                },
            ],
            "billing".to_string(),
            vec![("n1".to_string(), "Billing specialist".to_string())],
        ));
        app
    }

    #[test]
    fn router_routes_dialog_renders_routes_fallback_and_wiring() {
        let app = test_app_with_router_dialog();
        let theme = Theme::classic();
        let text = render_to_text(100, 30, |frame, _area| {
            draw_loop_editor_dialog(frame, &app, &theme);
        });

        assert!(text.contains("Router Routes"), "{text}");
        assert!(text.contains("Fallback:"), "{text}");
        assert!(text.contains("billing"), "{text}");
        assert!(text.contains("technical"), "{text}");
        assert!(text.contains("Billing specialist"), "{text}");
        assert!(text.contains("(unwired)"), "{text}");
        assert!(text.contains("[fallback]"), "{text}");
    }

    #[test]
    fn router_routes_dialog_shows_the_domain_validation_error_readably() {
        let mut app = test_app_with_router_dialog();
        app.loop_editor_dialog.as_mut().unwrap().parse_error =
            Some("A router must declare one route as fallback.".to_string());
        let theme = Theme::classic();
        let text = render_to_text(100, 30, |frame, _area| {
            draw_loop_editor_dialog(frame, &app, &theme);
        });

        assert!(
            text.contains("A router must declare one route as fallback."),
            "{text}"
        );
    }
}
