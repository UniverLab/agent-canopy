use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::centered_rect;
use super::ERROR_COLOR;
use crate::tui::app::types::App;
use crate::tui::ui::theme::Theme;

// Old function removed - using simple prompt dialog instead
pub(crate) fn draw_section_picker_modal(
    frame: &mut Frame,
    app: &App,
    accent: Color,
    mode: &crate::tui::app::dialog::SectionPickerMode,
    theme: &Theme,
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
                .borders(crate::tui::ui::borders_for(theme))
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
                Span::styled("↑↓ ", Style::default().fg(theme.dim_text)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("c ", Style::default().fg(theme.dim_text)),
                Span::styled("custom  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(theme.dim_text)),
                Span::styled("add  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(theme.dim_text)),
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
                .borders(crate::tui::ui::borders_for(theme))
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
                Style::default()
                    .fg(theme.header_color)
                    .bg(Color::Rgb(20, 35, 20)),
            )]);
            let input_area = ratatui::layout::Rect {
                x: inner.x + 1,
                y: inner.y + 1,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            frame.render_widget(Paragraph::new(input_line), input_area);

            let hint = Line::from(vec![
                Span::styled("Enter ", Style::default().fg(theme.dim_text)),
                Span::styled("add  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(theme.dim_text)),
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
                .borders(crate::tui::ui::borders_for(theme))
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
                Span::styled("↑↓ ", Style::default().fg(theme.dim_text)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(theme.dim_text)),
                Span::styled("remove  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(theme.dim_text)),
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
                .borders(crate::tui::ui::borders_for(theme))
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
                Span::styled("↑↓ ", Style::default().fg(theme.dim_text)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(theme.dim_text)),
                Span::styled("add  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(theme.dim_text)),
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
        SectionPickerMode::ProjectPicker {
            selected,
            entries,
            filter,
            scroll,
        } => {
            let filtered = crate::tui::app::dialog::SimplePromptDialog::filtered_project_indices(
                entries, filter,
            );
            let visible_rows =
                crate::tui::app::dialog::SimplePromptDialog::project_picker_visible_rows(
                    filtered.len(),
                );
            let height = crate::tui::app::dialog::SimplePromptDialog::project_picker_box_height(
                filtered.len(),
            );
            let area = centered_rect(60, height, frame.area());
            frame.render_widget(Clear, area);

            let title = if filtered.len() > visible_rows {
                format!(
                    " Project Context ({}/{}) ",
                    selected.saturating_add(1),
                    filtered.len()
                )
            } else {
                " Project Context ".to_string()
            };

            let block = Block::default()
                .title(title)
                .borders(crate::tui::ui::borders_for(theme))
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(Color::Rgb(10, 20, 30)));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            let mut filter_display = filter.clone();
            filter_display.push('│');
            let filter_line = Line::from(vec![
                Span::styled("  🔍 ", Style::default().fg(theme.dim_text)),
                Span::styled(filter_display, Style::default().fg(theme.header_color)),
            ]);
            frame.render_widget(
                Paragraph::new(filter_line),
                ratatui::layout::Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: 1,
                },
            );

            let list_y = inner.y + 1;
            if entries.is_empty() {
                let msg = Line::from(vec![Span::styled(
                    "  No registered projects found",
                    Style::default().fg(Color::DarkGray),
                )]);
                frame.render_widget(
                    Paragraph::new(msg),
                    ratatui::layout::Rect {
                        x: inner.x,
                        y: list_y,
                        width: inner.width,
                        height: 1,
                    },
                );
            } else if filtered.is_empty() {
                let msg = Line::from(vec![Span::styled(
                    "  no projects match",
                    Style::default().fg(Color::DarkGray),
                )]);
                frame.render_widget(
                    Paragraph::new(msg),
                    ratatui::layout::Rect {
                        x: inner.x,
                        y: list_y,
                        width: inner.width,
                        height: 1,
                    },
                );
            } else {
                let visible = filtered.iter().enumerate().skip(*scroll).take(visible_rows);
                for (y_pos, (i, &idx)) in (list_y..).zip(visible) {
                    if y_pos >= inner.y + inner.height.saturating_sub(1) {
                        break;
                    }
                    let Some(entry) = entries.get(idx) else {
                        continue;
                    };
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
                Span::styled("↑↓ ", Style::default().fg(theme.dim_text)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("type ", Style::default().fg(theme.dim_text)),
                Span::styled("filter  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(theme.dim_text)),
                Span::styled("add  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(theme.dim_text)),
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
        SectionPickerMode::PresetPicker {
            selected,
            entries,
            filter,
        } => {
            let filtered = crate::tui::app::dialog::SimplePromptDialog::filtered_preset_indices(
                entries, filter,
            );
            let height = (filtered.len() as u16 + 6).min(17);
            let area = centered_rect(60, height.max(7), frame.area());
            frame.render_widget(Clear, area);

            let block = Block::default()
                .title(" Preset ")
                .borders(crate::tui::ui::borders_for(theme))
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(Color::Rgb(10, 20, 30)));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            let mut filter_display = filter.clone();
            filter_display.push('│');
            let filter_line = Line::from(vec![
                Span::styled("  🔍 ", Style::default().fg(theme.dim_text)),
                Span::styled(filter_display, Style::default().fg(theme.header_color)),
            ]);
            frame.render_widget(
                Paragraph::new(filter_line),
                ratatui::layout::Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: 1,
                },
            );

            let list_y = inner.y + 1;
            if entries.is_empty() {
                let msg = Line::from(vec![Span::styled(
                    "  no presets in ~/.canopy/prompts",
                    Style::default().fg(Color::DarkGray),
                )]);
                frame.render_widget(
                    Paragraph::new(msg),
                    ratatui::layout::Rect {
                        x: inner.x,
                        y: list_y,
                        width: inner.width,
                        height: 1,
                    },
                );
            } else if filtered.is_empty() {
                let msg = Line::from(vec![Span::styled(
                    "  no presets match",
                    Style::default().fg(Color::DarkGray),
                )]);
                frame.render_widget(
                    Paragraph::new(msg),
                    ratatui::layout::Rect {
                        x: inner.x,
                        y: list_y,
                        width: inner.width,
                        height: 1,
                    },
                );
            } else {
                for (y_pos, (i, &idx)) in (list_y..).zip(filtered.iter().enumerate()) {
                    if y_pos >= inner.y + inner.height.saturating_sub(1) {
                        break;
                    }
                    let (name, preview, _) = &entries[idx];
                    let is_selected = i == *selected;
                    let style = if is_selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let display = if preview.is_empty() {
                        format!("  {name} ")
                    } else {
                        format!("  {name} — {preview} ")
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![Span::styled(display, style)])),
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
                Span::styled("↑↓ ", Style::default().fg(theme.dim_text)),
                Span::styled("select  ", Style::default().fg(Color::White)),
                Span::styled("type ", Style::default().fg(theme.dim_text)),
                Span::styled("filter  ", Style::default().fg(Color::White)),
                Span::styled("Enter ", Style::default().fg(theme.dim_text)),
                Span::styled("insert  ", Style::default().fg(Color::White)),
                Span::styled("Esc ", Style::default().fg(theme.dim_text)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::dialog::{SectionPickerMode, SimplePromptDialog};
    use crate::tui::app::types::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn make_app_with_prompt_dialog() -> App {
        use crate::db::Database;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(db, data_dir.path()).unwrap();
        app.simple_prompt_dialog = Some(SimplePromptDialog::new());
        app
    }

    fn render_to_text(
        width: u16,
        height: u16,
        draw: impl FnOnce(&mut ratatui::Frame, ratatui::layout::Rect),
    ) -> String {
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

    #[test]
    fn draw_section_picker_none_does_nothing() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(frame, &app, Color::Cyan, &SectionPickerMode::None, &theme);
        });
        // None mode should render nothing from the modal
        assert!(!text.contains("Add Section"));
    }

    #[test]
    fn draw_section_picker_no_dialog_returns_early() {
        let mut app = make_app_with_prompt_dialog();
        app.simple_prompt_dialog = None;
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::AddSection { selected: 0 },
                &theme,
            );
        });
        // Without dialog, nothing renders
        assert!(!text.contains("Add Section"));
    }

    #[test]
    fn draw_add_section_picker() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::AddSection { selected: 0 },
                &theme,
            );
        });
        assert!(
            text.contains("Add Section"),
            "Should render Add Section title: {text}"
        );
        assert!(text.contains("select"), "Should show hint: {text}");
    }

    #[test]
    fn draw_add_custom_section_picker() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::AddCustom {
                    input: "my_section".to_string(),
                },
                &theme,
            );
        });
        assert!(
            text.contains("Custom Section"),
            "Should render Custom Section title: {text}"
        );
        assert!(text.contains("my_section"), "Should show input: {text}");
    }

    #[test]
    fn draw_add_custom_section_picker_empty_input() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::AddCustom {
                    input: String::new(),
                },
                &theme,
            );
        });
        assert!(
            text.contains("Custom Section"),
            "Should render Custom Section title: {text}"
        );
        // Empty input still shows the cursor
        assert!(text.contains('│'), "Should show cursor: {text}");
    }

    #[test]
    fn draw_remove_section_picker() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::RemoveSection { selected: 0 },
                &theme,
            );
        });
        assert!(
            text.contains("Remove Section"),
            "Should render Remove Section title: {text}"
        );
        assert!(text.contains("cancel"), "Should show cancel hint: {text}");
    }

    #[test]
    fn draw_skills_picker() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let entries = vec![
            (
                "Skill A".to_string(),
                "skill_a".to_string(),
                "skill".to_string(),
            ),
            (
                "Skill B".to_string(),
                "skill_b".to_string(),
                "global".to_string(),
            ),
        ];
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::SkillsPicker {
                    selected: 0,
                    entries,
                    replace_id: None,
                },
                &theme,
            );
        });
        assert!(text.contains("Tools"), "Should render Tools title: {text}");
        assert!(text.contains("skill_a"), "Should show skill name: {text}");
    }

    #[test]
    fn draw_skills_picker_empty() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::SkillsPicker {
                    selected: 0,
                    entries: vec![],
                    replace_id: None,
                },
                &theme,
            );
        });
        assert!(
            text.contains("No skills found"),
            "Should show empty message: {text}"
        );
    }

    #[test]
    fn draw_project_picker() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let entries = vec![crate::tui::app::dialog::ProjectPickerEntry {
            name: "My Project".to_string(),
            hash: "abc123".to_string(),
            path: "/tmp/myproject".to_string(),
        }];
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::ProjectPicker {
                    selected: 0,
                    entries,
                    filter: String::new(),
                    scroll: 0,
                },
                &theme,
            );
        });
        assert!(
            text.contains("Project Context"),
            "Should render Project Context title: {text}"
        );
        assert!(
            text.contains("My Project"),
            "Should show project name: {text}"
        );
    }

    #[test]
    fn draw_project_picker_empty() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::ProjectPicker {
                    selected: 0,
                    entries: vec![],
                    filter: String::new(),
                    scroll: 0,
                },
                &theme,
            );
        });
        assert!(
            text.contains("No registered projects"),
            "Should show empty message: {text}"
        );
    }

    #[test]
    fn draw_project_picker_shows_filter_text() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let entries = vec![crate::tui::app::dialog::ProjectPickerEntry {
            name: "My Project".to_string(),
            hash: "abc123".to_string(),
            path: "/tmp/myproject".to_string(),
        }];
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::ProjectPicker {
                    selected: 0,
                    entries,
                    filter: "myp".to_string(),
                    scroll: 0,
                },
                &theme,
            );
        });
        assert!(text.contains("myp"), "Should show filter text: {text}");
    }

    #[test]
    fn draw_project_picker_filter_matches_nothing() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let entries = vec![crate::tui::app::dialog::ProjectPickerEntry {
            name: "My Project".to_string(),
            hash: "abc123".to_string(),
            path: "/tmp/myproject".to_string(),
        }];
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::ProjectPicker {
                    selected: 0,
                    entries,
                    filter: "zzz".to_string(),
                    scroll: 0,
                },
                &theme,
            );
        });
        assert!(
            text.contains("no projects match"),
            "Should show no-match message: {text}"
        );
    }

    #[test]
    fn draw_project_picker_scrolled_selection_stays_visible() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let entries: Vec<_> = (0..20)
            .map(|i| crate::tui::app::dialog::ProjectPickerEntry {
                name: format!("proj{i}"),
                hash: format!("h{i}"),
                path: format!("/proj{i}"),
            })
            .collect();
        let visible_rows =
            crate::tui::app::dialog::SimplePromptDialog::project_picker_visible_rows(20);
        let scroll = 20 - visible_rows;
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::ProjectPicker {
                    selected: 19,
                    entries: entries.clone(),
                    filter: String::new(),
                    scroll,
                },
                &theme,
            );
        });
        assert!(
            text.contains("proj19"),
            "Last entry must stay visible when scrolled: {text}"
        );
        assert!(
            !text.contains("proj0 "),
            "First entry should have scrolled out of view: {text}"
        );
        assert!(
            text.contains("(20/20)"),
            "Should show a scroll position indicator: {text}"
        );
    }

    #[test]
    fn draw_preset_picker() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let entries = vec![
            (
                "preset_a".to_string(),
                "Preview A".to_string(),
                String::new(),
            ),
            (
                "preset_b".to_string(),
                "Preview B".to_string(),
                String::new(),
            ),
        ];
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::PresetPicker {
                    selected: 0,
                    entries,
                    filter: String::new(),
                },
                &theme,
            );
        });
        assert!(
            text.contains("Preset"),
            "Should render Preset title: {text}"
        );
        assert!(text.contains("preset_a"), "Should show preset name: {text}");
    }

    #[test]
    fn draw_preset_picker_empty() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::PresetPicker {
                    selected: 0,
                    entries: vec![],
                    filter: String::new(),
                },
                &theme,
            );
        });
        assert!(
            text.contains("no presets"),
            "Should show empty message: {text}"
        );
    }

    #[test]
    fn draw_preset_picker_with_filter() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let entries = vec![
            (
                "preset_a".to_string(),
                "Preview A".to_string(),
                String::new(),
            ),
            (
                "preset_b".to_string(),
                "Preview B".to_string(),
                String::new(),
            ),
        ];
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::PresetPicker {
                    selected: 0,
                    entries,
                    filter: "aaa".to_string(),
                },
                &theme,
            );
        });
        assert!(text.contains("aaa"), "Should show filter text: {text}");
    }

    #[test]
    fn draw_preset_picker_filter_matches_nothing() {
        let app = make_app_with_prompt_dialog();
        let theme = Theme::classic();
        let entries = vec![(
            "preset_a".to_string(),
            "Preview A".to_string(),
            String::new(),
        )];
        let text = render_to_text(80, 24, |frame, _area| {
            draw_section_picker_modal(
                frame,
                &app,
                Color::Cyan,
                &SectionPickerMode::PresetPicker {
                    selected: 0,
                    entries,
                    filter: "zzz".to_string(),
                },
                &theme,
            );
        });
        assert!(
            text.contains("no presets match"),
            "Should show no match message: {text}"
        );
    }
}
