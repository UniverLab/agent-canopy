use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, ACCENT, DIM};
use crate::tui::app::types::{AgentEntry, App};
use crate::tui::ui::dialogs::at_picker::draw_at_picker_dropdown;
use crate::tui::ui::dialogs::section_picker::draw_section_picker_modal;

#[allow(unused_imports)]
use super::{BG_SELECTED, ERROR_COLOR, INTERACTIVE_COLOR};

/// Apply styling to collapsed paste blocks, making them stand out with accent color.
fn style_collapsed_paste_blocks(
    render_text: &str,
    accent: Color,
    _section_bg: Color,
) -> Vec<(String, Option<Color>)> {
    let mut result = Vec::new();
    let mut current_pos = 0;

    // Find all collapsed paste blocks: [Pasted ~N lines]
    while let Some(start) = render_text[current_pos..].find("[Pasted ~") {
        let start_abs = current_pos + start;

        // Add text before the block (uncolored)
        if start > 0 {
            result.push((render_text[current_pos..start_abs].to_string(), None));
        }

        // Find the end of the block
        if let Some(end_rel) = render_text[start_abs..].find(']') {
            let end_abs = start_abs + end_rel + 1;
            let block_text = &render_text[start_abs..end_abs];

            // Add the collapsed block with accent color
            result.push((block_text.to_string(), Some(accent)));
            current_pos = end_abs;
        } else {
            // No closing bracket, treat rest as normal
            result.push((render_text[start_abs..].to_string(), None));
            break;
        }
    }

    // Add remaining text
    if current_pos < render_text.len() {
        result.push((render_text[current_pos..].to_string(), None));
    }

    result
}

// Old function removed - using simple prompt dialog instead
fn generate_top_border(title: &str, width: u16, style: Style) -> Line<'static> {
    let title_with_spaces = format!(" {} ", title);
    let available_width = width.saturating_sub(title_with_spaces.len() as u16 + 2);
    let left_dashes = available_width / 2;
    let right_dashes = available_width - left_dashes;

    let border = format!(
        "┌{}{}{}┐",
        "─".repeat(left_dashes as usize),
        title_with_spaces,
        "─".repeat(right_dashes as usize)
    );
    Line::from(vec![Span::styled(border, style)])
}

/// Generate a bottom border line dynamically based on width
fn generate_bottom_border(width: u16, style: Style) -> Line<'static> {
    let border = format!("└{}┘", "─".repeat((width - 2) as usize));
    Line::from(vec![Span::styled(border, style)])
}
pub fn draw_simple_prompt_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = &app.simple_prompt_dialog else {
        return;
    };

    // Get agent accent color
    let accent = app
        .selected_agent()
        .and_then(|a| match a {
            AgentEntry::Interactive(idx) => {
                app.interactive_agents.get(*idx).map(|ia| ia.accent_color)
            }
            _ => None,
        })
        .unwrap_or(ACCENT);

    // Use 65% of terminal width (responsive, not edge-to-edge)
    let percent_x = 65u16;
    let dialog_width = (frame.area().width * percent_x / 100).max(40);
    let inner_width = dialog_width.saturating_sub(2);
    let field_width = inner_width.saturating_sub(2).max(10) as usize;

    // Pre-compute render height for each section (label + content + border + gap = content_h + 3).
    let section_heights: Vec<u16> = dialog
        .enabled_sections
        .iter()
        .enumerate()
        .map(|(i, section_name)| {
            let is_focused = dialog.focused_section == i;
            let content_h = if is_focused {
                let content = dialog.section_content_for_build(section_name).unwrap_or("");
                let vis = crate::tui::app::dialog::SimplePromptDialog::visual_line_count(
                    content,
                    field_width,
                );
                let max_h =
                    crate::tui::app::dialog::SimplePromptDialog::max_visible_lines(section_name);
                (vis as u16).clamp(1, max_h as u16)
            } else {
                1u16
            };
            content_h + 3 // label(1) + content + bottom_border(1) + gap(1)
        })
        .collect();

    let total_sections_height: u16 = section_heights.iter().sum();
    let total_height = 2 + 1 + 1 + total_sections_height + 1;

    // Cap dialog height — leave at least 4 rows margin, minimum 10 rows.
    let max_dialog_h = frame.area().height.saturating_sub(4).max(10);
    let height = total_height.min(max_dialog_h);

    let area = centered_rect(percent_x, height, frame.area());
    frame.render_widget(Clear, area);

    let title = " Prompt Builder ";
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Draw hint line
    let instructions = Line::from(vec![
        Span::styled("↑↓ ", Style::default().fg(DIM)),
        Span::styled("fields  ", Style::default().fg(Color::White)),
        Span::styled("Shift+↑↓ ", Style::default().fg(DIM)),
        Span::styled("navigate  ", Style::default().fg(Color::White)),
        Span::styled("@ ", Style::default().fg(DIM)),
        Span::styled("file  ", Style::default().fg(Color::White)),
        Span::styled("Ctrl+A ", Style::default().fg(DIM)),
        Span::styled("add/memory  ", Style::default().fg(Color::White)),
        Span::styled("Ctrl+X ", Style::default().fg(DIM)),
        Span::styled("remove  ", Style::default().fg(Color::White)),
        Span::styled("Ctrl+S ", Style::default().fg(DIM)),
        Span::styled("send  ", Style::default().fg(Color::White)),
        Span::styled("Esc  ", Style::default().fg(DIM)),
        Span::styled("hide", Style::default().fg(Color::White)),
    ]);

    let instructions_area = ratatui::layout::Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(instructions), instructions_area);

    // ── Scroll computation ─────────────────────────────────────────────────
    // sections_available_h = inner height minus hint(1) + blank(1).
    let sections_top = inner.y + 2;
    let sections_available_h = inner.height.saturating_sub(2);
    let mut picker_anchor_area: Option<ratatui::layout::Rect> = None;

    // Work backwards from focused_section to find the first section that fits.
    let start_idx = {
        let focused = dialog.focused_section;
        let focused_h = section_heights.get(focused).copied().unwrap_or(4);
        let mut remaining = sections_available_h.saturating_sub(focused_h);
        let mut start = focused;
        while start > 0 {
            let prev_h = section_heights.get(start - 1).copied().unwrap_or(4);
            if prev_h > remaining {
                break;
            }
            remaining -= prev_h;
            start -= 1;
        }
        start
    };

    // Scroll indicators
    let inner_bottom = inner.y + inner.height;
    if start_idx > 0 {
        let arrow = Span::styled(" ▲ ", Style::default().fg(accent));
        let a = ratatui::layout::Rect {
            x: inner.x,
            y: sections_top,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(arrow)).alignment(ratatui::layout::Alignment::Right),
            a,
        );
    }

    let mut y_pos = sections_top;

    // ── Draw all sections uniformly ─────────────────────────────────────────
    for (i, section_name) in dialog.enabled_sections.iter().enumerate() {
        // Skip sections before start_idx
        if i < start_idx {
            continue;
        }
        // Stop if we've run out of vertical space (leave 1 row for ▼ indicator)
        if y_pos + 3 >= inner_bottom {
            break;
        }

        let is_focused = dialog.focused_section == i;

        let section_type = {
            let known = [
                "tools",
                "instruction",
                "memory_context",
                "context",
                "project_context",
                "resources",
                "rag_search",
                "examples",
                "constraints",
            ];
            known
                .iter()
                .find(|k| section_name.starts_with(*k))
                .copied()
                .unwrap_or(section_name.as_str())
        };

        let label = crate::tui::app::dialog::SimplePromptDialog::get_available_sections()
            .into_iter()
            .find(|(name, _)| *name == section_type)
            .map(|(_, label)| label)
            .unwrap_or(section_type);

        let suffix = section_name.strip_prefix(section_type).unwrap_or("");
        let is_tools = section_type == "tools";
        let display_label = if is_tools && suffix.is_empty() {
            "Tools".to_string()
        } else if is_tools {
            format!("Tools {}", suffix.trim_start_matches('_'))
        } else if suffix.is_empty() {
            label.to_string()
        } else {
            format!("{} {}", label, suffix.trim_start_matches('_'))
        };

        let is_locked = dialog.is_locked(section_name);

        let display_label = if is_locked {
            format!("{display_label} 🔒")
        } else {
            display_label
        };

        let label_style = if is_focused {
            Style::default().fg(accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(accent)
        };

        let label_line = generate_top_border(&display_label, inner.width, label_style);
        let label_area = ratatui::layout::Rect {
            x: inner.x,
            y: y_pos,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(label_line), label_area);
        y_pos += 1;

        let section_bg = if is_focused {
            Color::Rgb(40, 40, 40)
        } else {
            Color::Rgb(30, 30, 30)
        };

        let content_raw = dialog
            .sections
            .get(section_name)
            .map(|s| s.as_str())
            .unwrap_or("");
        let content_real = dialog
            .collapsed_pastes
            .get(section_name)
            .map(|s| s.as_str())
            .unwrap_or(content_raw);

        let (render_text, cursor_idx_opt, content_height, scroll_offset) = if is_tools {
            // Tools section: read-only, always 1 line — shows skill label or placeholder
            let display = if content_raw.trim().is_empty() {
                "  (empty — Ctrl+A to pick a skill)".to_string()
            } else {
                content_raw.trim().to_string()
            };
            (display, None, 1u16, 0u16)
        } else if is_focused {
            let cursor_idx = dialog
                .cursor(section_name)
                .min(content_real.chars().count());
            let max_h =
                crate::tui::app::dialog::SimplePromptDialog::max_visible_lines(section_name);
            let vis = crate::tui::app::dialog::SimplePromptDialog::visual_line_count(
                content_real,
                field_width,
            );
            // Clamp content height to available space
            let max_avail = inner_bottom.saturating_sub(y_pos).saturating_sub(2);
            (
                content_real.to_string(),
                Some(cursor_idx),
                (vis as u16).clamp(1, max_h as u16).min(max_avail),
                dialog.scroll(section_name) as u16,
            )
        } else {
            let first_line = content_raw.lines().next().unwrap_or(content_raw);
            let text = if first_line.chars().count() > field_width {
                format!(
                    "{}…",
                    first_line
                        .chars()
                        .take(field_width.saturating_sub(1))
                        .collect::<String>()
                )
            } else {
                first_line.to_string()
            };
            (text, None, 1u16, 0u16)
        };

        let styled_content = if dialog.has_collapsed_paste(section_name) {
            // Use special styling for collapsed pastes to highlight with accent color
            style_collapsed_paste_blocks(&render_text, accent, section_bg)
        } else {
            // Use default file reference styling
            dialog.get_file_reference_with_styling(&render_text, accent)
        };
        let mut spans = Vec::new();
        if let Some(cursor_idx) = cursor_idx_opt {
            // Block cursor: highlight the char under the cursor without shifting text
            let mut char_count: usize = 0;
            for (text, color) in styled_content {
                let span_style = if let Some(c) = color {
                    Style::default().fg(c).bg(section_bg)
                } else {
                    Style::default().fg(Color::White).bg(section_bg)
                };
                let text_chars = text.chars().count();
                if cursor_idx >= char_count && cursor_idx < char_count + text_chars {
                    let local_pos = cursor_idx - char_count;
                    let before: String = text.chars().take(local_pos).collect();
                    let cursor_ch: String = text
                        .chars()
                        .nth(local_pos)
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| " ".to_string());
                    let after: String = text.chars().skip(local_pos + 1).collect();
                    if !before.is_empty() {
                        spans.push(Span::styled(before, span_style));
                    }
                    spans.push(Span::styled(
                        cursor_ch,
                        Style::default().fg(section_bg).bg(Color::White),
                    ));
                    if !after.is_empty() {
                        spans.push(Span::styled(after, span_style));
                    }
                } else {
                    spans.push(Span::styled(text, span_style));
                }
                char_count += text_chars;
            }
            if cursor_idx >= char_count {
                spans.push(Span::styled(
                    " ",
                    Style::default().fg(section_bg).bg(Color::White),
                ));
            }
        } else {
            for (text, color) in styled_content {
                let span_style = if let Some(c) = color {
                    Style::default().fg(c).bg(section_bg)
                } else {
                    Style::default().fg(Color::White).bg(section_bg)
                };
                spans.push(Span::styled(text, span_style));
            }
        }

        let content_paragraph = Paragraph::new(Line::from(spans))
            .wrap(ratatui::widgets::Wrap { trim: false })
            .scroll((scroll_offset, 0));

        let content_area = ratatui::layout::Rect {
            x: inner.x + 1,
            y: y_pos,
            width: inner.width.saturating_sub(2),
            height: content_height,
        };
        if is_focused {
            picker_anchor_area = Some(content_area);
        }
        frame.render_widget(content_paragraph, content_area);
        y_pos += content_height;

        let bottom_border = generate_bottom_border(inner.width, label_style);
        let border_area = ratatui::layout::Rect {
            x: inner.x,
            y: y_pos,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(bottom_border), border_area);
        y_pos += 2;
    }

    // ▼ indicator when there are more sections below
    let last_visible_section = {
        let mut last = start_idx;
        let mut yy = sections_top;
        for (i, _) in dialog.enabled_sections.iter().enumerate() {
            if i < start_idx {
                continue;
            }
            let sh = section_heights.get(i).copied().unwrap_or(4);
            if yy + sh >= inner_bottom {
                break;
            }
            yy += sh;
            last = i;
        }
        last
    };
    if last_visible_section < dialog.enabled_sections.len().saturating_sub(1) {
        let arrow = Span::styled(" ▼ ", Style::default().fg(accent));
        let a = ratatui::layout::Rect {
            x: inner.x,
            y: inner_bottom.saturating_sub(1),
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(arrow)).alignment(ratatui::layout::Alignment::Right),
            a,
        );
    }

    // Draw @ file picker dropdown if active
    if dialog.at_picker.is_some() {
        let anchor = picker_anchor_area.unwrap_or(inner);
        draw_at_picker_dropdown(frame, area, anchor, accent, dialog);
    }

    // Draw picker modal if open
    draw_section_picker_modal(frame, app, accent, &dialog.picker_mode);
}
