use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::{draw_dialog_left_wave, truncate_str};
use crate::tui::app::dialog::PromptTab;
use crate::tui::app::types::{AgentEntry, App};
use crate::tui::ui::dialogs::at_picker::draw_at_picker_dropdown;
use crate::tui::ui::dialogs::section_picker::draw_section_picker_modal;
use crate::tui::ui::theme::Theme;

/// Max rows the scheduled-sends list panel shows at once before it scrolls (B33).
const SCHEDULED_LIST_MAX_ROWS: usize = 4;

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
    text_color: Color,
) -> Vec<Line<'static>> {
    fn flush_run(spans: &mut Vec<Span<'static>>, run: &mut String, style: Style) {
        if !run.is_empty() {
            spans.push(Span::styled(std::mem::take(run), style));
        }
    }

    let field_width = field_width.max(1);
    let cursor_style = Style::default().fg(section_bg).bg(text_color);

    let mut lines: Vec<Line> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default().fg(text_color).bg(section_bg);
    let mut col = 0usize;
    let mut char_pos = 0usize;

    for (text, color) in styled_content {
        let base_style = Style::default()
            .fg(color.unwrap_or(text_color))
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

/// One entry in the prompt builder's shortcut/help bar.
struct ShortcutHint {
    /// Dim key label, includes its trailing space (e.g. `"Ctrl+A "`).
    key: String,
    /// White description, includes trailing padding (e.g. `"add section  "`).
    desc: String,
    /// Keep-priority: lower is more important. On a narrow window the
    /// highest-priority-number entries are dropped first. `Ctrl+S send` (1)
    /// and `Esc hide` (2) are never dropped.
    priority: u8,
}

impl ShortcutHint {
    fn width(&self) -> usize {
        self.key.chars().count() + self.desc.chars().count()
    }
}

/// All shortcut hints in left-to-right DISPLAY order. Priority (keep→drop):
/// Ctrl+S send, Esc hide, Shift+↑↓ navigate, Ctrl+L recall, Ctrl+P scheduled,
/// @ file, Ctrl+A add section, Ctrl+X remove. The `Ctrl+P scheduled` hint only
/// appears when the selected session actually has pending scheduled sends.
fn all_shortcut_hints(send_label: &str, send_hint: &str, has_scheduled: bool) -> Vec<ShortcutHint> {
    let mut hints = vec![
        ShortcutHint {
            key: "Shift+↑↓ ".to_string(),
            desc: "navigate  ".to_string(),
            priority: 3,
        },
        ShortcutHint {
            key: "@ ".to_string(),
            desc: "file  ".to_string(),
            priority: 5,
        },
        ShortcutHint {
            key: "Ctrl+A ".to_string(),
            desc: "add section  ".to_string(),
            priority: 6,
        },
        ShortcutHint {
            key: "Ctrl+X ".to_string(),
            desc: "remove  ".to_string(),
            priority: 7,
        },
        ShortcutHint {
            // Ctrl+E still works as an unadvertised alias.
            key: "Shift+←→ ".to_string(),
            desc: "tabs  ".to_string(),
            priority: 8,
        },
        ShortcutHint {
            key: "Ctrl+L ".to_string(),
            desc: "recall  ".to_string(),
            priority: 4,
        },
        ShortcutHint {
            key: send_label.to_string(),
            desc: format!("{send_hint}  "),
            priority: 1,
        },
        ShortcutHint {
            key: "Esc  ".to_string(),
            desc: "hide".to_string(),
            priority: 2,
        },
    ];
    // Advertise the scheduled-list entry point only when it does something —
    // inserted just before the send/hide pair so it reads next to recall.
    if has_scheduled {
        let insert_at = hints.len().saturating_sub(2);
        hints.insert(
            insert_at,
            ShortcutHint {
                key: "Ctrl+P ".to_string(),
                desc: "scheduled  ".to_string(),
                priority: 4,
            },
        );
    }
    hints
}

/// Pure function: pick which shortcut hints fit into `width` columns, in
/// display order, dropping the least important first. `Ctrl+S send` and
/// `Esc hide` (priority ≤ 2) are always kept even when they overflow.
fn select_shortcut_hints(
    width: usize,
    send_label: &str,
    send_hint: &str,
    has_scheduled: bool,
) -> Vec<ShortcutHint> {
    let mut hints = all_shortcut_hints(send_label, send_hint, has_scheduled);
    loop {
        let total: usize = hints.iter().map(ShortcutHint::width).sum();
        if total <= width {
            break;
        }
        // Drop the least important (highest priority number) droppable entry.
        let victim = hints
            .iter()
            .enumerate()
            .filter(|(_, hint)| hint.priority > 2)
            .max_by_key(|(_, hint)| hint.priority)
            .map(|(idx, _)| idx);
        match victim {
            Some(idx) => {
                hints.remove(idx);
            }
            None => break, // only the two mandatory hints remain
        }
    }
    hints
}

/// Draw the prompt builder. Returns the tab bar's origin `(x, y)` so the caller
/// can store it for mouse hit-testing the clickable Normal/Raw tabs, or `None`
/// when the dialog is not open.
pub fn draw_simple_prompt_dialog(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
) -> Option<((u16, u16), Option<ratatui::layout::Rect>)> {
    let dialog = app.simple_prompt_dialog.as_ref()?;

    // Get agent accent color
    let accent = app
        .selected_agent()
        .and_then(|a| match a {
            AgentEntry::Interactive(idx) => {
                app.interactive_agents.get(*idx).map(|ia| ia.accent_color)
            }
            _ => None,
        })
        .unwrap_or(theme.header_color);

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

    // Scheduled-sends list panel geometry (B33). The panel sits just above the
    // bottom send control, one row per pending send, and only exists when the
    // selected session actually has pending sends. A selection is only honored
    // (highlighted, scrolled into view) while there is a list to browse.
    let list_selected = dialog
        .scheduled_list_selected
        .filter(|_| !pending_scheduled.is_empty());
    let (list_visible_rows, list_scroll) =
        crate::tui::app::dialog::SimplePromptDialog::scheduled_list_view(
            pending_scheduled.len(),
            SCHEDULED_LIST_MAX_ROWS,
            list_selected,
        );
    // Panel height: a label/top-border row plus one row per visible entry.
    let list_panel_height: u16 = if pending_scheduled.is_empty() {
        0
    } else {
        1 + list_visible_rows as u16
    };

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

    // Raw tab: a single content block — the raw field, or the composed-prompt
    // preview when the buffer is empty — instead of the section stack.
    let raw_content_height: u16 = if dialog.active_tab == PromptTab::Raw {
        let text = if dialog.raw_is_empty() {
            dialog.raw_preview.as_deref().unwrap_or("")
        } else {
            dialog.raw_text()
        };
        let vis = crate::tui::app::dialog::SimplePromptDialog::visual_line_count(text, field_width);
        (vis as u16).clamp(3, 18)
    } else {
        0
    };
    let content_height = match dialog.active_tab {
        PromptTab::Normal => total_sections_height,
        PromptTab::Raw => raw_content_height,
    };
    // borders(2) + tab bar(1) + hint(1) + gap(1) + content + gap(1) +
    // scheduled-list panel + send(1). The panel is 0-height when there are no
    // pending sends, so the layout is byte-for-byte unchanged in that case.
    let total_height = 2 + 1 + 1 + 1 + content_height + 1 + list_panel_height + 1;

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
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(theme.dialog_bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // ── Tab bar (row 0) — clickable Normal / Raw windows ─────────────────
    // The active tab is drawn in accent (reversed for a clear "selected"
    // block); the inactive one is dim. Hit-boxes come from the same pure
    // geometry the mouse handler uses, so clicks land exactly on the labels.
    let tab_boxes = crate::tui::app::dialog::SimplePromptDialog::tab_hitboxes(inner.x, inner.y);
    let tab_spans: Vec<Span> = tab_boxes
        .iter()
        .map(|(tab, _)| {
            let label = match tab {
                PromptTab::Normal => crate::tui::app::dialog::TAB_NORMAL_LABEL,
                PromptTab::Raw => crate::tui::app::dialog::TAB_RAW_LABEL,
            };
            if *tab == dialog.active_tab {
                Span::styled(
                    label,
                    Style::default()
                        .fg(accent)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                )
            } else {
                Span::styled(label, Style::default().fg(theme.dim_text))
            }
        })
        .collect();
    let tab_area = ratatui::layout::Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(Line::from(tab_spans)), tab_area);

    // Draw hint line (row 1) — rendered by priority so the most important
    // shortcuts (Ctrl+S send, Esc hide) survive on narrow windows instead of
    // scrolling off the end.
    let (send_label, send_hint) = active_send_shortcut_label(app.keyboard_enhancement_active);
    let hint_spans: Vec<Span> = select_shortcut_hints(
        inner.width as usize,
        send_label,
        send_hint,
        !pending_scheduled.is_empty(),
    )
    .into_iter()
    .flat_map(|hint| {
        [
            Span::styled(hint.key, Style::default().fg(theme.dim_text)),
            Span::styled(hint.desc, Style::default().fg(theme.text_primary)),
        ]
    })
    .collect();
    let instructions = Line::from(hint_spans);

    let instructions_area = ratatui::layout::Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(instructions), instructions_area);

    // ── Send control (virtual focus index 0, U11) ───────────────────────
    // A single ghost line centered at the BOTTOM of the dialog — no box.
    // States: `send: now` / `send: date` selector (←→ toggles), inline
    // date-time picker while editing, `send: 2026-07-16 08:30` once picked.
    let send_is_focused = dialog.focused_section == 0 && !dialog.enabled_sections.is_empty();
    let send_y = inner.y + inner.height.saturating_sub(1);

    let mut send_spans: Vec<Span> = Vec::new();
    let ghost = Style::default().fg(theme.dim_text);
    let lit = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    let label_style = if send_is_focused { lit } else { ghost };

    if let Some(edit) = dialog.send_edit.as_ref() {
        // Inline picker: highlight the focused field.
        send_spans.push(Span::styled("send: ", label_style));
        let field_style = |idx: usize| {
            if edit.field == idx {
                Style::default()
                    .fg(theme.accent_fg)
                    .bg(accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_primary)
            }
        };
        let v = edit.value;
        send_spans.push(Span::styled(v.format("%Y").to_string(), field_style(0)));
        send_spans.push(Span::styled("-", ghost));
        send_spans.push(Span::styled(v.format("%m").to_string(), field_style(1)));
        send_spans.push(Span::styled("-", ghost));
        send_spans.push(Span::styled(v.format("%d").to_string(), field_style(2)));
        send_spans.push(Span::styled(" ", ghost));
        send_spans.push(Span::styled(v.format("%H").to_string(), field_style(3)));
        send_spans.push(Span::styled(":", ghost));
        send_spans.push(Span::styled(v.format("%M").to_string(), field_style(4)));
        send_spans.push(Span::styled(
            "  ↑↓ adjust · ←→ field · Enter ok · Esc cancel",
            ghost,
        ));
    } else if send_is_focused {
        send_spans.push(Span::styled("‹ ", ghost));
        send_spans.push(Span::styled(
            format!("send: {}", dialog.send_display()),
            lit,
        ));
        send_spans.push(Span::styled(" ›", ghost));
        let hint = match (dialog.send_choice, dialog.send_at) {
            (crate::tui::app::dialog::SendChoice::Date, None) => "  Enter: pick date & time",
            (crate::tui::app::dialog::SendChoice::Date, Some(_)) => {
                "  Enter: edit · Backspace: now"
            }
            _ => "  ←→ change",
        };
        send_spans.push(Span::styled(hint, ghost));
    } else {
        send_spans.push(Span::styled(
            format!("send: {}", dialog.send_display()),
            ghost,
        ));
    }

    if let Some(error) = dialog.send_error.as_ref() {
        send_spans.push(Span::styled(
            format!("  ⚠ {error}"),
            Style::default().fg(theme.warning),
        ));
    }
    // Pending scheduled sends now render as a dedicated LIST panel above the
    // send line (see `draw_scheduled_list`), not crammed onto this bar (B33).

    let send_text_width: usize = send_spans.iter().map(|s| s.content.chars().count()).sum();
    let send_x = inner.x
        + inner
            .width
            .saturating_sub(send_text_width as u16)
            .saturating_div(2);
    let send_area = ratatui::layout::Rect {
        x: send_x,
        y: send_y,
        width: inner.width.saturating_sub(send_x - inner.x),
        height: 1,
    };
    frame.render_widget(Paragraph::new(Line::from(send_spans)), send_area);

    // ── Content region ──────────────────────────────────────────────────
    // The Raw tab draws a single free-text field (or the composed-prompt
    // preview when empty); the Normal tab draws its scrolling section stack.
    let mut picker_anchor_area: Option<ratatui::layout::Rect> = None;
    if dialog.active_tab == PromptTab::Raw {
        draw_raw_tab_content(
            frame,
            dialog,
            accent,
            inner,
            field_width,
            list_panel_height,
            theme,
        );
    } else {
        // sections_available_h = inner height minus tab(1) + hint(1) + gap(1) at the
        // top and gap(1) + send line(1) at the bottom, and the scheduled-list
        // panel (B33) reserved just above the send line.
        let sections_top = inner.y + 3;
        let sections_available_h = inner.height.saturating_sub(5 + list_panel_height);

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

        // Scroll indicators. The effective bottom excludes the scheduled-list
        // panel (B33) so sections never draw over it.
        let inner_bottom = inner.y + inner.height.saturating_sub(list_panel_height);
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
                theme.field_bg_focused
            } else {
                theme.field_bg
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
            let wrapped_lines = wrap_styled_content(
                styled_content,
                cursor_idx_opt,
                field_width,
                section_bg,
                theme.text_primary,
            );
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
    } // end Normal-tab content region

    // ── Scheduled-sends list panel (B33) ─────────────────────────────────
    // A compact list of pending scheduled sends, one row each, docked just
    // above the send line. Navigable/selectable via keyboard (Ctrl+P).
    if list_panel_height > 0 {
        let panel_top = inner.y + inner.height.saturating_sub(1 + list_panel_height);
        draw_scheduled_list(
            frame,
            &pending_scheduled,
            list_selected,
            list_scroll,
            list_visible_rows,
            dialog.editing_scheduled_id.as_deref(),
            accent,
            inner,
            panel_top,
            theme,
        );
    }

    // Draw @ file picker dropdown if active
    if dialog.at_picker.is_some() {
        let anchor = picker_anchor_area.unwrap_or(inner);
        draw_at_picker_dropdown(frame, area, anchor, accent, dialog, theme);
    }

    // Draw picker modal if open
    draw_section_picker_modal(frame, app, accent, &dialog.picker_mode, theme);

    let content_rect = if dialog.active_tab == PromptTab::Raw {
        Some(
            crate::tui::app::dialog::SimplePromptDialog::raw_content_rect(inner, list_panel_height),
        )
    } else {
        None
    };
    Some(((inner.x, inner.y), content_rect))
}

/// Draw the Raw tab's content region: a single free-text field spanning the
/// content area, or — when the buffer is empty — a dimmed, read-only preview
/// of the composed Normal-form prompt (the exact string a send would produce),
/// clamped with a "… (+N lines)" tail when it overflows.
#[allow(clippy::too_many_arguments)]
fn draw_raw_tab_content(
    frame: &mut Frame,
    dialog: &crate::tui::app::dialog::SimplePromptDialog,
    accent: Color,
    inner: ratatui::layout::Rect,
    field_width: usize,
    list_panel_height: u16,
    theme: &Theme,
) {
    let content_top = inner.y + 3;
    // Leave the send line plus the scheduled-list panel (B33) at the bottom.
    let content_bottom = inner.y + inner.height.saturating_sub(2 + list_panel_height);
    let avail_h = content_bottom.saturating_sub(content_top).max(1) as usize;

    // Header/label line (row 2, the gap row above the field).
    let raw_empty = dialog.raw_is_empty();
    let label = if raw_empty {
        " Raw — preview of composed prompt (read-only) "
    } else {
        " Raw — sent exactly as typed "
    };
    let label_style = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(generate_top_border(label, inner.width, label_style)),
        ratatui::layout::Rect {
            x: inner.x,
            y: content_top.saturating_sub(1),
            width: inner.width,
            height: 1,
        },
    );

    let content_area = ratatui::layout::Rect {
        x: inner.x + 1,
        y: content_top,
        width: inner.width.saturating_sub(2),
        height: avail_h as u16,
    };
    let section_bg = theme.field_bg;

    if raw_empty {
        // Read-only preview, dimmed. ↑↓/PgUp/PgDn scroll it; edge markers show
        // how much is hidden above and below the window.
        let preview = dialog
            .raw_preview
            .as_deref()
            .unwrap_or("(nothing to preview yet)");
        let all_lines: Vec<&str> = preview.lines().collect();
        let scroll = dialog
            .raw_preview_scroll
            .min(all_lines.len().saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        if scroll > 0 {
            lines.push(Line::from(Span::styled(
                format!("… (−{scroll} lines above)"),
                Style::default().fg(accent),
            )));
        }
        let body_h = avail_h.saturating_sub(lines.len()).saturating_sub(1).max(1);
        let shown = all_lines.len().min(scroll + body_h) - scroll;
        lines.extend(all_lines.iter().skip(scroll).take(body_h).map(|l| {
            Line::from(Span::styled(
                (*l).to_string(),
                Style::default().fg(theme.dim_text),
            ))
        }));
        let below = all_lines.len().saturating_sub(scroll + shown);
        if below > 0 {
            lines.push(Line::from(Span::styled(
                format!("… (+{below} lines, ↑↓ scroll)"),
                Style::default().fg(accent),
            )));
        }
        frame.render_widget(
            Paragraph::new(ratatui::text::Text::from(lines)),
            content_area,
        );
    } else {
        // Editable raw field: reuse the section wrapping/cursor machinery.
        let text = dialog.raw_text();
        let cursor_idx = dialog
            .cursor(crate::tui::app::dialog::RAW_SECTION_ID)
            .min(text.chars().count());
        // Cursor-follow scroll computed from the cursor's visual line,
        // overridden by wheel scroll when the user has scrolled with the mouse.
        let prefix: String = text.chars().take(cursor_idx).collect();
        let cursor_line =
            crate::tui::app::dialog::SimplePromptDialog::visual_line_count(&prefix, field_width)
                .saturating_sub(1);
        let base_scroll = cursor_line.saturating_sub(avail_h.saturating_sub(1));
        let scroll = dialog.raw_edit_scroll.unwrap_or(base_scroll) as u16;

        let styled = dialog.get_file_reference_with_styling(text, accent);
        let wrapped = wrap_styled_content(
            styled,
            Some(cursor_idx),
            field_width,
            section_bg,
            theme.text_primary,
        );
        frame.render_widget(
            Paragraph::new(ratatui::text::Text::from(wrapped)).scroll((scroll, 0)),
            content_area,
        );
    }
}

/// One scheduled-list row's text: `MM-DD HH:MM  <first line of prompt>`,
/// truncated to `width`. Pure so the rendered row and its tests agree.
fn scheduled_row_text(fire_local: chrono::NaiveDateTime, prompt: &str, width: usize) -> String {
    let preview = prompt.lines().next().unwrap_or("").trim();
    let full = format!("{}  {}", fire_local.format("%m-%d %H:%M"), preview);
    truncate_str(&full, width)
}

/// Draw the pending-scheduled-sends list panel (B33): a labeled top border,
/// then one row per visible entry (fire time + prompt preview). The selected
/// row (only set while the list has keyboard focus) is drawn reversed.
#[allow(clippy::too_many_arguments)]
fn draw_scheduled_list(
    frame: &mut Frame,
    pending: &[crate::db::scheduled_sends::ScheduledSend],
    selected: Option<usize>,
    scroll: usize,
    visible_rows: usize,
    editing_id: Option<&str>,
    accent: Color,
    inner: ratatui::layout::Rect,
    panel_top: u16,
    theme: &Theme,
) {
    let editing = editing_id.is_some_and(|id| pending.iter().any(|send| send.id == id));
    let label = if editing {
        format!(" Scheduled sends ({}) — editing ", pending.len())
    } else {
        format!(" Scheduled sends ({}) · Ctrl+P browse ", pending.len())
    };
    let label_style = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(generate_top_border(&label, inner.width, label_style)),
        ratatui::layout::Rect {
            x: inner.x,
            y: panel_top,
            width: inner.width,
            height: 1,
        },
    );

    let row_width = inner.width.saturating_sub(2) as usize;
    for row in 0..visible_rows {
        let idx = scroll + row;
        let Some(send) = pending.get(idx) else {
            break;
        };
        let fire_local = send.fire_at.with_timezone(&chrono::Local).naive_local();
        let text = scheduled_row_text(fire_local, &send.prompt, row_width);
        let is_selected = selected == Some(idx);
        let style = if is_selected {
            Style::default()
                .fg(accent)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(theme.text_primary)
        };
        let marker = if is_selected { "› " } else { "  " };
        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(accent)),
            Span::styled(text, style),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            ratatui::layout::Rect {
                x: inner.x,
                y: panel_top + 1 + row as u16,
                width: inner.width,
                height: 1,
            },
        );
    }
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
        let lines = wrap_styled_content(styled, None, 40, Color::Black, Color::White);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec!["hola mundo", "segunda línea"]);
    }

    #[test]
    fn wrap_matches_visual_line_count_on_overflow() {
        use crate::tui::app::dialog::SimplePromptDialog;
        let content = "una frase que se pasa del ancho del campo";
        let width = 10;
        let lines = wrap_styled_content(
            vec![(content.to_string(), None)],
            None,
            width,
            Color::Black,
            Color::White,
        );
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
        let lines = wrap_styled_content(styled, Some(2), 40, Color::Black, Color::White);
        assert_eq!(line_text(&lines[0]), "ab ");
    }

    fn hint_keys(width: usize) -> Vec<String> {
        select_shortcut_hints(width, "Ctrl+S ", "send", false)
            .iter()
            .map(|hint| hint.key.trim().to_string())
            .collect()
    }

    fn hint_keys_scheduled(width: usize) -> Vec<String> {
        select_shortcut_hints(width, "Ctrl+S ", "send", true)
            .iter()
            .map(|hint| hint.key.trim().to_string())
            .collect()
    }

    #[test]
    fn shortcut_bar_shows_every_hint_at_full_width() {
        let keys = hint_keys(200);
        assert_eq!(
            keys,
            vec![
                "Shift+↑↓",
                "@",
                "Ctrl+A",
                "Ctrl+X",
                "Shift+←→",
                "Ctrl+L",
                "Ctrl+S",
                "Esc"
            ]
        );
        // The removed "↑↓ fields" entry must not reappear.
        assert!(!keys.iter().any(|k| k == "↑↓"));
    }

    #[test]
    fn shortcut_bar_advertises_ctrl_p_only_when_sends_are_pending() {
        // With pending sends, Ctrl+P appears (between recall and send); without,
        // it never does.
        let with = hint_keys_scheduled(200);
        assert_eq!(
            with,
            vec![
                "Shift+↑↓",
                "@",
                "Ctrl+A",
                "Ctrl+X",
                "Shift+←→",
                "Ctrl+L",
                "Ctrl+P",
                "Ctrl+S",
                "Esc"
            ]
        );
        assert!(!hint_keys(200).iter().any(|k| k == "Ctrl+P"));
    }

    #[test]
    fn scheduled_row_text_shows_time_and_truncated_preview() {
        let fire = chrono::NaiveDate::from_ymd_opt(2026, 7, 20)
            .unwrap()
            .and_hms_opt(14, 5, 0)
            .unwrap();
        // Only the first line of the prompt is previewed.
        let row = scheduled_row_text(fire, "run the build\nthen deploy", 40);
        assert_eq!(row, "07-20 14:05  run the build");
        // Narrow widths truncate with an ellipsis and never overflow.
        let narrow = scheduled_row_text(fire, "a very long prompt line here", 18);
        assert!(narrow.chars().count() <= 18);
        assert!(narrow.starts_with("07-20 14:05"));
    }

    #[test]
    fn shortcut_bar_keeps_only_send_and_hide_when_very_narrow() {
        // Even below the width the two mandatory hints need, they stay visible.
        assert_eq!(hint_keys(10), vec!["Ctrl+S", "Esc"]);
        assert_eq!(hint_keys(1), vec!["Ctrl+S", "Esc"]);
    }

    #[test]
    fn shortcut_bar_drops_least_important_first_at_medium_width() {
        // 45 cols fits navigate(19) + send(13) + hide(9) = 41, but not recall.
        let keys = hint_keys(45);
        assert_eq!(keys, vec!["Shift+↑↓", "Ctrl+S", "Esc"]);
        assert!(!keys.iter().any(|k| k == "@"));
        assert!(!keys.iter().any(|k| k == "Ctrl+L"));
    }

    #[test]
    fn shortcut_bar_preserves_display_order_after_dropping_middle_hints() {
        // 60 cols keeps navigate + recall but not @ file (dropped by priority),
        // and the survivors stay in left-to-right display order.
        let keys = hint_keys(60);
        assert_eq!(keys, vec!["Shift+↑↓", "Ctrl+L", "Ctrl+S", "Esc"]);
    }

    #[test]
    fn shortcut_bar_always_keeps_the_two_mandatory_hints() {
        for width in [0usize, 5, 22, 50, 99, 300] {
            let keys = hint_keys(width);
            assert!(keys.contains(&"Ctrl+S".to_string()), "width {width}");
            assert!(keys.contains(&"Esc".to_string()), "width {width}");
        }
    }

    #[test]
    fn generate_top_border_basic() {
        let line = generate_top_border(" Title ", 20, Style::default());
        assert_eq!(line.width(), 20);
    }

    #[test]
    fn generate_top_border_empty_title() {
        let line = generate_top_border("", 10, Style::default());
        assert_eq!(line.width(), 10);
    }

    #[test]
    fn generate_top_border_narrow() {
        let line = generate_top_border("Hi", 4, Style::default());
        assert_eq!(line.width(), 4);
    }

    #[test]
    fn generate_bottom_border_basic() {
        let line = generate_bottom_border(20, Style::default());
        assert_eq!(line.width(), 20);
    }

    #[test]
    fn generate_bottom_border_narrow() {
        let line = generate_bottom_border(3, Style::default());
        assert_eq!(line.width(), 3);
    }

    #[test]
    fn style_collapsed_paste_blocks_basic() {
        let result = style_collapsed_paste_blocks(
            "line1\n[Pasted ~3 lines]\nline3",
            Color::Black,
            Color::Black,
        );
        assert!(!result.is_empty());
    }

    #[test]
    fn style_collapsed_paste_blocks_no_paste() {
        let result = style_collapsed_paste_blocks("hello world", Color::Black, Color::Black);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "hello world");
        assert!(result[0].1.is_none());
    }

    #[test]
    fn style_collapsed_paste_blocks_empty() {
        let result = style_collapsed_paste_blocks("", Color::Black, Color::Black);
        assert!(result.is_empty());
    }

    #[test]
    fn centered_rect_fixed_basic() {
        let area = ratatui::layout::Rect::new(0, 0, 100, 40);
        let result = centered_rect_fixed(50, 10, area);
        assert!(result.width <= 50);
        assert_eq!(result.height, 10);
    }

    #[test]
    fn centered_rect_fixed_full_width() {
        let area = ratatui::layout::Rect::new(0, 0, 100, 40);
        let result = centered_rect_fixed(100, 5, area);
        assert_eq!(result.height, 5);
    }

    #[test]
    fn wrap_empty_content() {
        // The function always pushes at least one line
        let lines = wrap_styled_content(vec![], None, 40, Color::Black, Color::White);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn wrap_single_short_line() {
        let lines = wrap_styled_content(
            vec![("hello".to_string(), None)],
            None,
            40,
            Color::Black,
            Color::White,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "hello");
    }

    #[test]
    fn scheduled_row_text_short_prompt() {
        let fire = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let row = scheduled_row_text(fire, "short", 40);
        assert!(row.contains("09:00"));
        assert!(row.contains("short"));
    }

    #[test]
    fn shortcut_bar_zero_width() {
        let keys = hint_keys(0);
        assert!(keys.contains(&"Ctrl+S".to_string()));
        assert!(keys.contains(&"Esc".to_string()));
    }

    #[test]
    fn wrap_with_style() {
        let styled = vec![("hello".to_string(), Some(Color::Red))];
        let lines = wrap_styled_content(styled, None, 40, Color::Black, Color::White);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn generate_top_border_long_title() {
        let line =
            generate_top_border("A Very Long Title That Exceeds Width", 10, Style::default());
        assert_eq!(line.width(), 10);
    }

    #[test]
    fn wrap_preserves_newline_at_end() {
        let styled = vec![("hello\n".to_string(), None)];
        let lines = wrap_styled_content(styled, None, 40, Color::Black, Color::White);
        // "hello" on first line, empty second line
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "hello");
    }

    #[test]
    fn wrap_multiple_newlines() {
        let styled = vec![("a\nb\nc".to_string(), None)];
        let lines = wrap_styled_content(styled, None, 40, Color::Black, Color::White);
        assert_eq!(lines.len(), 3);
        assert_eq!(line_text(&lines[0]), "a");
        assert_eq!(line_text(&lines[1]), "b");
        assert_eq!(line_text(&lines[2]), "c");
    }

    #[test]
    fn wrap_tab_expands_to_spaces() {
        let styled = vec![("a\tb".to_string(), None)];
        let lines = wrap_styled_content(styled, None, 40, Color::Black, Color::White);
        let text = line_text(&lines[0]);
        assert!(text.contains("b"));
        // Tab should be expanded to spaces
        assert!(text.len() > 3);
    }

    #[test]
    fn wrap_tab_at_width_boundary() {
        let styled = vec![("abcd\tef".to_string(), None)];
        let lines = wrap_styled_content(styled, None, 5, Color::Black, Color::White);
        // Tab after "abcd" (col 4) wraps to next line
        assert!(lines.len() >= 2);
    }

    #[test]
    fn wrap_cursor_in_middle() {
        let styled = vec![("hello".to_string(), None)];
        let lines = wrap_styled_content(styled, Some(2), 40, Color::Black, Color::White);
        // Cursor at position 2 should be visible
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn wrap_cursor_at_start() {
        let styled = vec![("hello".to_string(), None)];
        let lines = wrap_styled_content(styled, Some(0), 40, Color::Black, Color::White);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn wrap_styled_content_with_accent_color() {
        let styled = vec![
            ("hello ".to_string(), Some(Color::Red)),
            ("world".to_string(), Some(Color::Blue)),
        ];
        let lines = wrap_styled_content(styled, None, 40, Color::Black, Color::White);
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "hello world");
    }

    #[test]
    fn centered_rect_fixed_wider_than_area() {
        let area = ratatui::layout::Rect::new(0, 0, 50, 20);
        let result = centered_rect_fixed(100, 10, area);
        // Should be clamped to area width
        assert!(result.width <= 50);
    }

    #[test]
    fn centered_rect_fixed_taller_than_area() {
        let area = ratatui::layout::Rect::new(0, 0, 100, 10);
        let result = centered_rect_fixed(50, 20, area);
        // Should be clamped to area height
        assert!(result.height <= 10);
    }

    #[test]
    fn centered_rect_fixed_zero_dimensions() {
        let area = ratatui::layout::Rect::new(0, 0, 0, 0);
        let result = centered_rect_fixed(10, 5, area);
        // Should handle gracefully
        assert!(result.width >= 1);
        assert!(result.height >= 1);
    }

    #[test]
    fn active_send_shortcut_both_variants() {
        let (key1, desc1) = active_send_shortcut_label(true);
        let (key2, desc2) = active_send_shortcut_label(false);
        assert_eq!(desc1, "send");
        assert_eq!(desc2, "send");
        assert_ne!(key1, key2);
    }

    #[test]
    fn all_shortcut_hints_includes_mandatory() {
        let hints = all_shortcut_hints("Ctrl+S ", "send", false);
        let keys: Vec<String> = hints.iter().map(|h| h.key.trim().to_string()).collect();
        assert!(keys.contains(&"Ctrl+S".to_string()));
        assert!(keys.contains(&"Esc".to_string()));
    }

    #[test]
    fn all_shortcut_hints_with_scheduled() {
        let hints = all_shortcut_hints("Ctrl+S ", "send", true);
        let keys: Vec<String> = hints.iter().map(|h| h.key.trim().to_string()).collect();
        assert!(keys.contains(&"Ctrl+P".to_string()));
    }

    #[test]
    fn select_shortcut_hints_tiny_width() {
        let hints = select_shortcut_hints(1, "Ctrl+S ", "send", false);
        let keys: Vec<String> = hints.iter().map(|h| h.key.trim().to_string()).collect();
        assert!(keys.contains(&"Ctrl+S".to_string()));
        assert!(keys.contains(&"Esc".to_string()));
    }

    #[test]
    fn style_collapsed_paste_blocks_unclosed_bracket() {
        let result = style_collapsed_paste_blocks(
            "text [Pasted ~3 lines more text here",
            Color::Red,
            Color::Black,
        );
        // No closing bracket, so rest is treated as normal text
        assert!(!result.is_empty());
    }

    #[test]
    fn style_collapsed_paste_blocks_multiple_pastes() {
        let result = style_collapsed_paste_blocks(
            "[Pasted ~2 lines] and [Pasted ~3 lines]",
            Color::Red,
            Color::Black,
        );
        assert!(!result.is_empty());
    }

    #[test]
    fn scheduled_row_text_empty_prompt() {
        let fire = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let row = scheduled_row_text(fire, "", 40);
        assert!(row.contains("00:00"));
    }

    #[test]
    fn scheduled_row_text_multiline_prompt() {
        let fire = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(12, 30, 0)
            .unwrap();
        let row = scheduled_row_text(fire, "first line\nsecond line\nthird line", 40);
        assert!(row.contains("first line"));
        assert!(!row.contains("second line"));
    }

    #[test]
    fn shortcut_hint_width() {
        let hint = ShortcutHint {
            key: "Ctrl+S ".to_string(),
            desc: "send  ".to_string(),
            priority: 1,
        };
        // "Ctrl+S " (7 chars) + "send  " (6 chars) = 13
        assert_eq!(hint.width(), 13);
    }

    #[test]
    fn wrap_content_exactly_field_width() {
        let styled = vec![("abcde".to_string(), None)];
        let lines = wrap_styled_content(styled, None, 5, Color::Black, Color::White);
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "abcde");
    }

    #[test]
    fn wrap_content_one_over_field_width() {
        let styled = vec![("abcdef".to_string(), None)];
        let lines = wrap_styled_content(styled, None, 5, Color::Black, Color::White);
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "abcde");
        assert_eq!(line_text(&lines[1]), "f");
    }

    #[test]
    fn wrap_content_empty_segments() {
        let styled = vec![(String::new(), None), ("hello".to_string(), None)];
        let lines = wrap_styled_content(styled, None, 40, Color::Black, Color::White);
        assert_eq!(line_text(&lines[0]), "hello");
    }

    /// active_send_shortcut_label returns keyboard-enhancement-dependent labels
    #[test]
    fn active_send_shortcut_label_with_enhancement() {
        let (key, hint) = active_send_shortcut_label(true);
        assert_eq!(key, "Shift+Enter ");
        assert_eq!(hint, "send");
    }

    /// active_send_shortcut_label returns Ctrl+S when enhancement is off
    #[test]
    fn active_send_shortcut_label_without_enhancement() {
        let (key, hint) = active_send_shortcut_label(false);
        assert_eq!(key, "Ctrl+S ");
        assert_eq!(hint, "send");
    }

    /// all_shortcut_hints always includes core shortcuts
    #[test]
    fn all_shortcut_hints_includes_core_shortcuts() {
        let hints = all_shortcut_hints("Ctrl+S ", "send", false);
        // Should have at least 8 entries (without scheduled)
        assert!(hints.len() >= 8);
        // Find key hints
        let has_ctrl_s = hints.iter().any(|h| h.key.contains("Ctrl+S"));
        let has_esc = hints.iter().any(|h| h.key.contains("Esc"));
        assert!(has_ctrl_s, "should include Ctrl+S");
        assert!(has_esc, "should include Esc");
    }

    /// all_shortcut_hints includes Ctrl+P only when has_scheduled is true
    #[test]
    fn all_shortcut_hints_ctrl_p_conditional() {
        let without = all_shortcut_hints("Ctrl+S ", "send", false);
        let with = all_shortcut_hints("Ctrl+S ", "send", true);

        let without_ctrl_p = without.iter().any(|h| h.key.contains("Ctrl+P"));
        let with_ctrl_p = with.iter().any(|h| h.key.contains("Ctrl+P"));

        assert!(
            !without_ctrl_p,
            "should not have Ctrl+P when has_scheduled=false"
        );
        assert!(with_ctrl_p, "should have Ctrl+P when has_scheduled=true");
    }

    /// all_shortcut_hints uses provided send_label and send_hint
    #[test]
    fn all_shortcut_hints_uses_provided_labels() {
        let hints = all_shortcut_hints("Custom ", "action", false);
        let send_hint = hints
            .iter()
            .find(|h| h.key == "Custom ")
            .expect("should have custom send key");
        assert_eq!(send_hint.desc, "action  ");
    }

    /// select_shortcut_hints keeps core hints even when narrow
    #[test]
    fn select_shortcut_hints_keeps_core_shortcuts() {
        // Very narrow width should still keep send and hide
        let hints = select_shortcut_hints(5, "Ctrl+S ", "send", false);
        let priorities: Vec<_> = hints.iter().map(|h| h.priority).collect();
        // Should still have priorities 1 and 2 (send and hide)
        assert!(priorities.contains(&1), "should keep send (priority 1)");
        assert!(priorities.contains(&2), "should keep hide (priority 2)");
    }

    /// select_shortcut_hints returns all hints when width is large
    #[test]
    fn select_shortcut_hints_all_fit_when_wide() {
        let all = all_shortcut_hints("Ctrl+S ", "send", true);
        let selected = select_shortcut_hints(500, "Ctrl+S ", "send", true);
        assert_eq!(
            selected.len(),
            all.len(),
            "all hints should fit when width=500"
        );
    }

    /// all_shortcut_hints priorities are maintained
    #[test]
    fn all_shortcut_hints_have_valid_priorities() {
        let hints = all_shortcut_hints("Ctrl+S ", "send", true);
        for hint in &hints {
            // Priority 1 and 2 are reserved for send/hide
            // Others should be 3+
            assert!(hint.priority > 0, "all priorities should be positive");
        }
        // Send should have priority 1
        let send = hints.iter().find(|h| h.key.contains("Ctrl+S"));
        assert_eq!(send.map(|h| h.priority), Some(1));
        // Hide should have priority 2
        let hide = hints.iter().find(|h| h.key.contains("Esc"));
        assert_eq!(hide.map(|h| h.priority), Some(2));
    }
}
