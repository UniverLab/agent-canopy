use crate::tui::app::types::App;
use crate::tui::terminal_history::SessionHistory;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

fn ghost_for(
    histories: &std::collections::HashMap<String, SessionHistory>,
    session_name: &str,
    input: &str,
) -> Option<String> {
    histories
        .get(session_name)
        .and_then(|h| h.ghost_suggestion(input))
        .map(str::to_string)
}

/// Base input box height (rows) when the buffer holds up to `BASE_LINE_CAPACITY`
/// wrapped lines — matches the padding rows the single-line box always had.
const BASE_HEIGHT: u16 = 4;
/// Lines that already fit within `BASE_HEIGHT` without growing the box.
const BASE_LINE_CAPACITY: usize = 3;
/// Hard cap so the input box never eats the whole panel.
const MAX_HEIGHT: u16 = 8;

/// Split `text` into visual rows at `width` columns. Breaks at explicit `\n`
/// and hard-wraps whichever segment overflows `width`; byte ranges are exact
/// slices of `text` so no content is rewritten (multi-space runs, etc. survive).
pub(crate) fn wrap_offsets(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut seg_start = 0usize;
    let bytes_len = text.len();

    for segment in text.split('\n') {
        let seg_len = segment.len();
        if segment.is_empty() {
            lines.push((seg_start, seg_start));
        } else {
            let mut line_start = seg_start;
            let mut col = 0usize;
            for (i, _ch) in segment.char_indices() {
                if col == width {
                    lines.push((line_start, seg_start + i));
                    line_start = seg_start + i;
                    col = 0;
                }
                col += 1;
            }
            lines.push((line_start, seg_start + seg_len));
        }
        seg_start += seg_len;
        if seg_start < bytes_len {
            seg_start += 1; // skip the '\n' separator
        }
    }

    lines
}

/// Height (rows) the warp input box needs to show `text` wrapped at `width`
/// columns: free up to `BASE_LINE_CAPACITY` lines, then grow 1:1 up to `MAX_HEIGHT`.
pub(crate) fn input_height(text: &str, width: u16) -> u16 {
    let total_lines = wrap_offsets(text, width.max(1) as usize).len();
    let extra = total_lines.saturating_sub(BASE_LINE_CAPACITY) as u16;
    (BASE_HEIGHT + extra).min(MAX_HEIGHT)
}

/// Map a byte cursor position to (wrapped line index, column) using the same
/// offsets `wrap_offsets` produced, so rendering and cursor placement never disagree.
fn cursor_line_col(offsets: &[(usize, usize)], text: &str, cursor_pos: usize) -> (usize, usize) {
    let mut best = 0;
    for (idx, &(start, _end)) in offsets.iter().enumerate() {
        if start <= cursor_pos {
            best = idx;
        } else {
            break;
        }
    }
    let (start, _) = offsets[best];
    let col = text[start..cursor_pos.max(start)].chars().count();
    (best, col)
}

pub fn draw_warp_input_box(frame: &mut Frame, area: Rect, app: &App, idx: usize) {
    let Some(agent) = app.terminal_agents.get(idx) else {
        return;
    };

    let cwd = compact_cwd(&agent.working_dir);
    let raw_input_text = agent
        .input_buffer
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default();
    let sensitive_input = agent.is_sensitive_input_active();
    let input_text = if sensitive_input {
        String::new()
    } else {
        raw_input_text
    };
    let cursor_pos = agent.warp_cursor.min(input_text.len());

    let accent = agent.accent_color;
    // Field-style input box (no border, darkgray background — matches the
    // opencode chat input look). The split layout reserves enough rows for
    // the wrapped content; we use them all so the field has visible
    // top/bottom padding around the text rows.
    let input_bg = Color::Rgb(20, 20, 28);
    let block = Block::default().style(Style::default().bg(input_bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width < 4 {
        return;
    }

    // Every text span inside the field must carry the field's background
    // explicitly, otherwise ratatui falls back to the terminal default
    // and the text appears to float on the wrong color.
    let bg = input_bg;
    let prompt_style = |fg: Color, bold: bool| {
        let mut s = Style::default().bg(bg).fg(fg);
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        s
    };

    // Prompt indicator: compact cwd + chevron. Continuation lines are
    // indented by the same width so wrapped/typed text stays aligned.
    let prompt = format!("{} ❯ ", cwd);
    let prompt_len = prompt.chars().count() as u16;
    let indent = " ".repeat(prompt_len as usize);
    let content_width = inner.width.saturating_sub(prompt_len).max(1) as usize;

    if sensitive_input {
        let line = Line::from(vec![
            Span::styled(&prompt, prompt_style(accent, true)),
            Span::styled(
                "[hidden input]",
                prompt_style(Color::Rgb(180, 180, 120), false),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), inner);
        let cx = inner.x + prompt_len;
        if cx < inner.x + inner.width {
            frame.set_cursor_position((cx, inner.y));
        }
        return;
    }

    if input_text.is_empty() {
        let line = Line::from(vec![
            Span::styled(&prompt, prompt_style(accent, true)),
            Span::styled(
                "type a command…",
                prompt_style(Color::Rgb(80, 80, 100), false),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), inner);
        let cx = inner.x + prompt_len;
        if cx < inner.x + inner.width {
            frame.set_cursor_position((cx, inner.y));
        }
        return;
    }

    let offsets = wrap_offsets(&input_text, content_width);
    let (cursor_line, cursor_col) = cursor_line_col(&offsets, &input_text, cursor_pos);

    // Vertical scroll: keep the cursor's row visible within inner.height rows.
    let visible_rows = inner.height as usize;
    let scroll_start = if offsets.len() > visible_rows {
        cursor_line
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(offsets.len() - visible_rows)
    } else {
        0
    };

    let ghost = if cursor_pos == input_text.len() {
        ghost_for(&app.terminal_histories, &agent.name, &input_text)
    } else {
        None
    };

    let mut lines: Vec<Line> = Vec::new();
    for (row_idx, &(start, end)) in offsets
        .iter()
        .enumerate()
        .skip(scroll_start)
        .take(visible_rows)
    {
        let text_slice = &input_text[start..end];
        let mut spans = if row_idx == 0 {
            vec![Span::styled(prompt.clone(), prompt_style(accent, true))]
        } else {
            vec![Span::styled(indent.clone(), prompt_style(bg, false))]
        };
        spans.push(Span::styled(
            text_slice.to_string(),
            prompt_style(Color::White, false),
        ));

        // Inline ghost-text suggestion on the last visual row, only when the
        // cursor sits at the very end of the buffer — otherwise accepting it
        // would interleave with the current cursor position, which is surprising.
        if row_idx == offsets.len() - 1 {
            if let Some(ghost) = &ghost {
                let suffix = &ghost[input_text.len()..];
                if !suffix.is_empty() {
                    let used_prefix = if row_idx == 0 {
                        prompt_len as usize
                    } else {
                        indent.chars().count()
                    };
                    let used = used_prefix + text_slice.chars().count();
                    let max_suffix = (inner.width as usize).saturating_sub(used);
                    let visible_suffix: String = suffix.chars().take(max_suffix).collect();
                    if !visible_suffix.is_empty() {
                        spans.push(Span::styled(
                            visible_suffix,
                            prompt_style(Color::Rgb(110, 110, 130), false)
                                .add_modifier(Modifier::ITALIC),
                        ));
                    }
                }
            }
        }

        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), inner);

    if cursor_line >= scroll_start && cursor_line < scroll_start + visible_rows {
        let row_on_screen = (cursor_line - scroll_start) as u16;
        let col_prefix = if cursor_line == 0 {
            prompt_len
        } else {
            indent.chars().count() as u16
        };
        let cx = inner.x + col_prefix + cursor_col as u16;
        let cy = inner.y + row_on_screen;
        if cx < inner.x + inner.width && cy < inner.y + inner.height {
            frame.set_cursor_position((cx, cy));
        }
    }
}

pub fn render_command_chips(frame: &mut Frame, area: Rect, app: &App, session_name: &str) {
    let hist = match app.terminal_histories.get(session_name) {
        Some(h) if !h.commands.is_empty() => h,
        _ => return,
    };

    // Get last 5 unique commands, most recent first
    let mut recent: Vec<&str> = Vec::new();
    let mut sorted: Vec<&crate::tui::terminal_history::CommandEntry> =
        hist.commands.iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.last_run));
    for entry in &sorted {
        if !recent.contains(&entry.cmd.as_str()) {
            recent.push(&entry.cmd);
        }
        if recent.len() >= 5 {
            break;
        }
    }
    if recent.is_empty() {
        return;
    }

    // Build chip spans that fit in the available width
    let bar_y = area.y + area.height.saturating_sub(1);
    let max_w = area.width as usize;
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0;

    for cmd in &recent {
        let chip = format!(" ✓ {} ", cmd);
        let chip_len = chip.chars().count() + 1; // +1 for gap
        if used + chip_len > max_w {
            break;
        }
        spans.push(Span::styled(
            chip,
            Style::default()
                .fg(Color::Rgb(180, 220, 180))
                .bg(Color::Rgb(20, 40, 20)),
        ));
        spans.push(Span::raw(" "));
        used += chip_len;
    }

    if !spans.is_empty() {
        let bar = Paragraph::new(Line::from(spans));
        let bar_area = Rect::new(area.x, bar_y, area.width, 1);
        frame.render_widget(bar, bar_area);
    }
}

pub fn compact_cwd(cwd: &str) -> String {
    let mut path = cwd.to_string();

    // Replace home dir with ~
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            path = format!("~{rest}");
        }
    }

    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 3 {
        return path;
    }

    // Show first + last segment with … in between
    let first = parts[0];
    let last = parts[parts.len() - 1];
    if first.starts_with('~') {
        format!("{first}/…/{last}")
    } else {
        format!("/{first}/…/{last}")
    }
}

#[cfg(test)]
mod tests {
    use super::{cursor_line_col, input_height, wrap_offsets};

    fn wrapped_text(text: &str, width: usize) -> Vec<&str> {
        wrap_offsets(text, width)
            .into_iter()
            .map(|(s, e)| &text[s..e])
            .collect()
    }

    #[test]
    fn wrap_offsets_hard_wraps_long_unbroken_text() {
        let text = "a".repeat(100);
        assert_eq!(
            wrapped_text(&text, 40),
            vec!["a".repeat(40), "a".repeat(40), "a".repeat(20)]
        );
    }

    #[test]
    fn wrap_offsets_preserves_exact_text_across_explicit_newlines() {
        let text = "cd  foo\nls -la";
        assert_eq!(wrapped_text(text, 40), vec!["cd  foo", "ls -la"]);
    }

    #[test]
    fn wrap_offsets_empty_text_is_single_empty_line() {
        assert_eq!(wrapped_text("", 40), vec![""]);
    }

    #[test]
    fn cursor_line_col_tracks_position_across_wrapped_lines() {
        let text = "a".repeat(100);
        let offsets = wrap_offsets(&text, 40);
        assert_eq!(cursor_line_col(&offsets, &text, 0), (0, 0));
        assert_eq!(cursor_line_col(&offsets, &text, 40), (1, 0));
        assert_eq!(cursor_line_col(&offsets, &text, 45), (1, 5));
        assert_eq!(cursor_line_col(&offsets, &text, 100), (2, 20));
    }

    #[test]
    fn input_height_matches_wrap_offsets_line_count() {
        let text = "a".repeat(200);
        let height = input_height(&text, 40);
        assert_eq!(wrap_offsets(&text, 40).len(), 5);
        assert_eq!(height, 6); // 5 lines - 3 free = 2 extra rows over base(4)
    }
}
