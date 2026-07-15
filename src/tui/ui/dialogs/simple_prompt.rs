use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{draw_dialog_left_wave, truncate_str, ACCENT, DIM};
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

/// Wrap styled content into visual lines with the exact char-based math of
/// `SimplePromptDialog::visual_line_count`, so rendered text, box height, and
/// cursor/scroll positions always agree. Hard newlines break lines (ratatui
/// drops `\n` inside a `Line` as a control char, which visually glued words
/// together), overflow wraps at `field_width`, and tabs expand to 4-col stops.
/// The char at `cursor_idx` is drawn as a block cursor.
fn wrap_styled_content(
    styled_content: Vec<(String, Option<Color>)>,
    cursor_idx: Option<usize>,
    field_width: usize,
    section_bg: Color,
) -> Vec<Line<'static>> {
    fn flush_run(spans: &mut Vec<Span<'static>>, run: &mut String, style: Style) {
        if !run.is_empty() {
            spans.push(Span::styled(std::mem::take(run), style));
        }
    }

    let field_width = field_width.max(1);
    let cursor_style = Style::default().fg(section_bg).bg(Color::White);

    let mut lines: Vec<Line> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default().fg(Color::White).bg(section_bg);
    let mut col = 0usize;
    let mut char_pos = 0usize;

    for (text, color) in styled_content {
        let base_style = Style::default()
            .fg(color.unwrap_or(Color::White))
            .bg(section_bg);
        for ch in text.chars() {
            let style = if cursor_idx == Some(char_pos) {
                cursor_style
            } else {
                base_style
            };
            match ch {
                '\n' => {
                    flush_run(&mut spans, &mut run, run_style);
                    if style == cursor_style {
                        spans.push(Span::styled(" ", cursor_style));
                    }
                    lines.push(Line::from(std::mem::take(&mut spans)));
                    col = 0;
                }
                '\t' => {
                    let tab = 4 - (col % 4);
                    if col + tab > field_width {
                        flush_run(&mut spans, &mut run, run_style);
                        lines.push(Line::from(std::mem::take(&mut spans)));
                        col = tab;
                    } else {
                        col += tab;
                    }
                    if style != run_style {
                        flush_run(&mut spans, &mut run, run_style);
                        run_style = style;
                    }
                    run.push_str(&" ".repeat(tab));
                }
                _ => {
                    if col + 1 > field_width {
                        flush_run(&mut spans, &mut run, run_style);
                        lines.push(Line::from(std::mem::take(&mut spans)));
                        col = 1;
                    } else {
                        col += 1;
                    }
                    if style != run_style {
                        flush_run(&mut spans, &mut run, run_style);
                        run_style = style;
                    }
                    run.push(ch);
                }
            }
            char_pos += 1;
        }
    }

    flush_run(&mut spans, &mut run, run_style);
    // Cursor past the end of content: draw it as a highlighted blank cell.
    if cursor_idx == Some(char_pos) {
        spans.push(Span::styled(" ", cursor_style));
    }
    lines.push(Line::from(spans));
    lines
}

// Old function removed - using simple prompt dialog instead
fn generate_top_border(title: &str, width: u16, style: Style) -> Line<'static> {
    if width < 2 {
        return Line::from(vec![Span::styled(String::new(), style)]);
    }

    let max_title_chars = width.saturating_sub(4) as usize;
    let title_with_spaces = if max_title_chars == 0 {
        String::new()
    } else {
        format!(" {} ", truncate_str(title, max_title_chars))
    };
    let title_width = title_with_spaces.chars().count() as u16;
    let available_width = width.saturating_sub(title_width + 2);
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
    if width < 2 {
        return Line::from(vec![Span::styled(String::new(), style)]);
    }
    let border = format!("└{}┘", "─".repeat((width - 2) as usize));
    Line::from(vec![Span::styled(border, style)])
}

fn centered_rect_fixed(
    width: u16,
    height: u16,
    area: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let clamped_w = width.clamp(1, area.width.max(1));
    let clamped_h = height.clamp(1, area.height.max(1));
    let x = area.x + area.width.saturating_sub(clamped_w) / 2;
    let y = area.y + area.height.saturating_sub(clamped_h) / 2;
    ratatui::layout::Rect::new(x, y, clamped_w, clamped_h)
}

/// Which send shortcut is currently active, for the footer/hint line.
/// Shift+Enter requires the terminal's Kitty keyboard enhancement protocol
/// to disambiguate it from plain Enter; where that isn't supported, Ctrl+S
/// remains the fallback (see `run_tui`'s `supports_keyboard_enhancement`
/// probe at startup).
fn active_send_shortcut_label(keyboard_enhancement_active: bool) -> (&'static str, &'static str) {
    if keyboard_enhancement_active {
        ("Shift+Enter ", "send")
    } else {
        ("Ctrl+S ", "send")
    }
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

    // Pending scheduled sends targeting the currently selected session —
    // shown next to the Schedule field and cancelable with Ctrl+K there.
    let selected_session_id = app.selected_agent().and_then(|a| match a {
        AgentEntry::Interactive(idx) => app.interactive_agents.get(*idx).map(|ia| ia.id.clone()),
        _ => None,
    });
    let pending_scheduled = selected_session_id
        .as_deref()
        .and_then(|id| app.db.list_pending_scheduled_sends_for_session(id).ok())
        .unwrap_or_default();

    // Use 65% of terminal width (responsive, not edge-to-edge)
    let percent_x = 65u16;
    let frame_area = frame.area();
    let max_dialog_w = frame_area.width.saturating_sub(2).max(1);
    let preferred_dialog_w = frame_area.width.saturating_mul(percent_x) / 100;
    let min_dialog_w = 40u16.min(max_dialog_w);
    let dialog_width = preferred_dialog_w.clamp(min_dialog_w, max_dialog_w);
    let inner_width = dialog_width.saturating_sub(2);
    let field_width = inner_width.saturating_sub(2).max(10) as usize;

    // Pre-compute render height for each section (label + content + border + gap = content_h + 3).
    // focused_section 0 = send_at, sections start at index 1.
    let section_focus_offset = 1;
    let section_heights: Vec<u16> = dialog
        .enabled_sections
        .iter()
        .enumerate()
        .map(|(i, section_name)| {
            let is_focused = dialog.focused_section == i + section_focus_offset;
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
    let max_dialog_h = frame_area.height.saturating_sub(2).max(1);
    let min_dialog_h = 10u16.min(max_dialog_h);
    let height = total_height.min(max_dialog_h);
    let height = height.max(min_dialog_h);

    let area = centered_rect_fixed(dialog_width, height, frame_area);
    frame.render_widget(Clear, area);
    draw_dialog_left_wave(frame, area, app.animation_tick.into());

    let title = " Prompt Builder ";
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Draw hint line
    let (send_label, send_hint) = active_send_shortcut_label(app.keyboard_enhancement_active);
    let instructions = Line::from(vec![
        Span::styled("↑↓ ", Style::default().fg(DIM)),
        Span::styled("fields  ", Style::default().fg(Color::White)),
        Span::styled("Shift+↑↓ ", Style::default().fg(DIM)),
        Span::styled("navigate  ", Style::default().fg(Color::White)),
        Span::styled("@ ", Style::default().fg(DIM)),
        Span::styled("file  ", Style::default().fg(Color::White)),
        Span::styled("Ctrl+A ", Style::default().fg(DIM)),
        Span::styled("add section  ", Style::default().fg(Color::White)),
        Span::styled("Ctrl+X ", Style::default().fg(DIM)),
        Span::styled("remove  ", Style::default().fg(Color::White)),
        Span::styled("Ctrl+L ", Style::default().fg(DIM)),
        Span::styled("recall  ", Style::default().fg(Color::White)),
        Span::styled(send_label, Style::default().fg(DIM)),
        Span::styled(format!("{send_hint}  "), Style::default().fg(Color::White)),
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

    // ── Send-at field (virtual section at focus index 0) ────────────────
    let send_at_y = inner.y + 1;
    let send_at_is_focused = dialog.focused_section == 0 && !dialog.enabled_sections.is_empty();
    let send_at_bg = if send_at_is_focused {
        Color::Rgb(40, 40, 40)
    } else {
        Color::Rgb(30, 30, 30)
    };
    let send_at_label_style = if send_at_is_focused {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(accent)
    };
    let send_at_label = generate_top_border("Schedule", inner.width, send_at_label_style);
    let send_at_label_area = ratatui::layout::Rect {
        x: inner.x,
        y: send_at_y,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(send_at_label), send_at_label_area);

    // Render send_at content
    let send_at_display = dialog.send_at_display();
    let send_at_hint = " ↑↓ adjust  ←→ unit  Backspace clear";
    let pending_suffix = match pending_scheduled.first() {
        Some(next) => {
            let next_local = next.fire_at.with_timezone(&chrono::Local);
            format!(
                "   {} scheduled → {} (Ctrl+K cancel)",
                pending_scheduled.len(),
                next_local.format("%H:%M")
            )
        }
        None => String::new(),
    };
    let send_at_text = format!("  {send_at_display}{send_at_hint}{pending_suffix}");
    let send_at_content_style = Style::default()
        .fg(if send_at_is_focused {
            Color::White
        } else {
            DIM
        })
        .bg(send_at_bg);
    let send_at_paragraph = Paragraph::new(Line::from(Span::styled(
        send_at_text,
        send_at_content_style,
    )));
    let send_at_content_area = ratatui::layout::Rect {
        x: inner.x + 1,
        y: send_at_y + 1,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    frame.render_widget(send_at_paragraph, send_at_content_area);

    let send_at_border = generate_bottom_border(inner.width, send_at_label_style);
    let send_at_border_area = ratatui::layout::Rect {
        x: inner.x,
        y: send_at_y + 2,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(send_at_border), send_at_border_area);

    // ── Scroll computation ─────────────────────────────────────────────────
    // sections_available_h = inner height minus hint(1) + send_at(3) + blank(1).
    let sections_top = send_at_y + 4;
    let sections_available_h = inner.height.saturating_sub(5);
    let mut picker_anchor_area: Option<ratatui::layout::Rect> = None;

    // Work backwards from focused_section to find the first section that fits.
    // focused_section 0 = send_at (handled above), sections start at index 1.
    let section_focus_offset = 1; // send_at occupies focus index 0
    let start_idx = {
        let focused = dialog.focused_section.saturating_sub(section_focus_offset);
        let focused = focused.min(dialog.enabled_sections.len().saturating_sub(1));
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

        let is_focused = dialog.focused_section == i + section_focus_offset;

        let section_type = {
            let known = [
                "tools",
                "instruction",
                "context",
                "project_context",
                "resources",
                "rag_search",
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
            format!("{display_label} [locked]")
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
        let wrapped_lines =
            wrap_styled_content(styled_content, cursor_idx_opt, field_width, section_bg);
        let content_paragraph =
            Paragraph::new(ratatui::text::Text::from(wrapped_lines)).scroll((scroll_offset, 0));

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

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn active_send_shortcut_prefers_shift_enter_when_supported() {
        assert_eq!(active_send_shortcut_label(true), ("Shift+Enter ", "send"));
    }

    #[test]
    fn active_send_shortcut_falls_back_to_ctrl_s_when_unsupported() {
        assert_eq!(active_send_shortcut_label(false), ("Ctrl+S ", "send"));
    }

    #[test]
    fn wrap_preserves_spaces_and_breaks_on_newline() {
        let styled = vec![("hola mundo\nsegunda línea".to_string(), None)];
        let lines = wrap_styled_content(styled, None, 40, Color::Black);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec!["hola mundo", "segunda línea"]);
    }

    #[test]
    fn wrap_matches_visual_line_count_on_overflow() {
        use crate::tui::app::dialog::SimplePromptDialog;
        let content = "una frase que se pasa del ancho del campo";
        let width = 10;
        let lines =
            wrap_styled_content(vec![(content.to_string(), None)], None, width, Color::Black);
        assert_eq!(
            lines.len(),
            SimplePromptDialog::visual_line_count(content, width)
        );
        // No characters are lost or altered by wrapping.
        let joined: String = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(joined, content.replace('\n', ""));
    }

    #[test]
    fn wrap_renders_cursor_at_end_as_blank_cell() {
        let styled = vec![("ab".to_string(), None)];
        let lines = wrap_styled_content(styled, Some(2), 40, Color::Black);
        assert_eq!(line_text(&lines[0]), "ab ");
    }
}
