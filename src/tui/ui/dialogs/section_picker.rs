use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, ACCENT, DIM};
use crate::tui::app::types::App;

#[allow(unused_imports)]
use super::{BG_SELECTED, ERROR_COLOR, INTERACTIVE_COLOR};

// Old function removed - using simple prompt dialog instead
pub(crate) fn draw_section_picker_modal(
    frame: &mut Frame,
    app: &App,
    accent: Color,
    mode: &crate::tui::app::dialog::SectionPickerMode,
) {
    use crate::tui::app::dialog::SectionPickerMode;

    let Some(dialog) = &app.simple_prompt_dialog else {
        return;
    };

    match mode {
        SectionPickerMode::AddSection { selected } => {
            let addable = dialog.get_addable_sections();
            let height = (addable.len() as u16 + 4).min(15);
            let area = centered_rect(50, height, frame.area());
            frame.render_widget(Clear, area);

            let title = " Add Section ";
            let block = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(Color::Rgb(15, 25, 15)));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            for (y_pos, (i, (_, label))) in (inner.y..).zip(addable.iter().enumerate()) {
                let is_selected = i == *selected;
                let style = if is_selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let line = Line::from(vec![Span::styled(format!("  {} ", label), style)]);
                let line_area = ratatui::layout::Rect {
                    x: inner.x,
                    y: y_pos,
                    width: inner.width,
                    height: 1,
                };
                frame.render_widget(Paragraph::new(line), line_area);
            }

            let hint = Line::from(vec![
                Span::styled("↑↓ ", Style::default().fg(DIM)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("c ", Style::default().fg(DIM)),
                Span::styled("custom  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(DIM)),
                Span::styled("add  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(DIM)),
                Span::styled("cancel", Style::default().fg(Color::White)),
            ]);
            let hint_area = ratatui::layout::Rect {
                x: inner.x,
                y: inner.y + inner.height.saturating_sub(1),
                width: inner.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(hint), hint_area);
        }
        SectionPickerMode::AddCustom { input } => {
            let area = centered_rect(50, 6, frame.area());
            frame.render_widget(Clear, area);

            let title = " Custom Section ";
            let block = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(Color::Rgb(15, 25, 15)));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            let label_line = Line::from(vec![Span::styled("Name: ", Style::default().fg(accent))]);
            let label_area = ratatui::layout::Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(label_line), label_area);

            let mut display = input.clone();
            display.push('│');
            let input_line = Line::from(vec![Span::styled(
                display,
                Style::default().fg(ACCENT).bg(Color::Rgb(20, 35, 20)),
            )]);
            let input_area = ratatui::layout::Rect {
                x: inner.x + 1,
                y: inner.y + 1,
                width: inner.width - 2,
                height: 1,
            };
            frame.render_widget(Paragraph::new(input_line), input_area);

            let hint = Line::from(vec![
                Span::styled("Enter ", Style::default().fg(DIM)),
                Span::styled("add  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(DIM)),
                Span::styled("cancel", Style::default().fg(Color::White)),
            ]);
            let hint_area = ratatui::layout::Rect {
                x: inner.x,
                y: inner.y + 3,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(hint), hint_area);
        }
        SectionPickerMode::RemoveSection { selected } => {
            let removable = dialog.get_removable_sections();
            let height = (removable.len() as u16 + 4).min(15);
            let area = centered_rect(50, height, frame.area());
            frame.render_widget(Clear, area);

            let title = " Remove Section ";
            let block = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(Color::Rgb(15, 25, 15)));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            for (y_pos, (i, (_, display_label))) in (inner.y..).zip(removable.iter().enumerate()) {
                let is_selected = i == *selected;

                let style = if is_selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(ERROR_COLOR)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let line = Line::from(vec![Span::styled(format!("  {} ", display_label), style)]);
                let line_area = ratatui::layout::Rect {
                    x: inner.x,
                    y: y_pos,
                    width: inner.width,
                    height: 1,
                };
                frame.render_widget(Paragraph::new(line), line_area);
            }

            let hint = Line::from(vec![
                Span::styled("↑↓ ", Style::default().fg(DIM)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(DIM)),
                Span::styled("remove  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(DIM)),
                Span::styled("cancel", Style::default().fg(Color::White)),
            ]);
            let hint_area = ratatui::layout::Rect {
                x: inner.x,
                y: inner.y + inner.height.saturating_sub(1),
                width: inner.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(hint), hint_area);
        }
        SectionPickerMode::SkillsPicker {
            selected, entries, ..
        } => {
            let height = (entries.len() as u16 + 5).min(16);
            let area = centered_rect(55, height, frame.area());
            frame.render_widget(Clear, area);

            let block = Block::default()
                .title(" Tools — Pick a Skill ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(Color::Rgb(10, 20, 30)));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            if entries.is_empty() {
                let msg = Line::from(vec![Span::styled(
                    "  No skills found",
                    Style::default().fg(Color::DarkGray),
                )]);
                frame.render_widget(
                    Paragraph::new(msg),
                    ratatui::layout::Rect {
                        x: inner.x,
                        y: inner.y,
                        width: inner.width,
                        height: 1,
                    },
                );
            } else {
                for (y_pos, (i, (_label, raw_name, prefix))) in
                    (inner.y..).zip(entries.iter().enumerate())
                {
                    if y_pos >= inner.y + inner.height.saturating_sub(1) {
                        break;
                    }
                    let is_selected = i == *selected;
                    let style = if is_selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    // Picker shows [prefix]:name for clarity (skill vs global)
                    let display = format!("  [{prefix}]:{raw_name} ");
                    let line = Line::from(vec![Span::styled(display, style)]);
                    frame.render_widget(
                        Paragraph::new(line),
                        ratatui::layout::Rect {
                            x: inner.x,
                            y: y_pos,
                            width: inner.width,
                            height: 1,
                        },
                    );
                }
            }

            let hint = Line::from(vec![
                Span::styled("↑↓ ", Style::default().fg(DIM)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(DIM)),
                Span::styled("add  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(DIM)),
                Span::styled("cancel", Style::default().fg(Color::White)),
            ]);
            frame.render_widget(
                Paragraph::new(hint),
                ratatui::layout::Rect {
                    x: inner.x,
                    y: inner.y + inner.height.saturating_sub(1),
                    width: inner.width,
                    height: 1,
                },
            );
        }
        SectionPickerMode::ProjectPicker { selected, entries } => {
            let height = (entries.len() as u16 + 5).min(16);
            let area = centered_rect(60, height.max(6), frame.area());
            frame.render_widget(Clear, area);

            let block = Block::default()
                .title(" Project Context ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(Color::Rgb(10, 20, 30)));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            if entries.is_empty() {
                let msg = Line::from(vec![Span::styled(
                    "  No registered projects found",
                    Style::default().fg(Color::DarkGray),
                )]);
                frame.render_widget(
                    Paragraph::new(msg),
                    ratatui::layout::Rect {
                        x: inner.x,
                        y: inner.y,
                        width: inner.width,
                        height: 1,
                    },
                );
            } else {
                for (y_pos, (i, entry)) in (inner.y..).zip(entries.iter().enumerate()) {
                    if y_pos >= inner.y + inner.height.saturating_sub(1) {
                        break;
                    }
                    let is_selected = i == *selected;
                    let style = if is_selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let display = format!("  {} [{}] ", entry.name, entry.hash);
                    let line = Line::from(vec![Span::styled(display, style)]);
                    frame.render_widget(
                        Paragraph::new(line),
                        ratatui::layout::Rect {
                            x: inner.x,
                            y: y_pos,
                            width: inner.width,
                            height: 1,
                        },
                    );
                }
            }

            let hint = Line::from(vec![
                Span::styled("↑↓ ", Style::default().fg(DIM)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(DIM)),
                Span::styled("add  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(DIM)),
                Span::styled("cancel", Style::default().fg(Color::White)),
            ]);
            frame.render_widget(
                Paragraph::new(hint),
                ratatui::layout::Rect {
                    x: inner.x,
                    y: inner.y + inner.height.saturating_sub(1),
                    width: inner.width,
                    height: 1,
                },
            );
        }
        SectionPickerMode::None => {}
    }
}
