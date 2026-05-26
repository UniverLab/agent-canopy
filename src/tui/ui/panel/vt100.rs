use crate::tui::agent::sanitize::line_looks_sensitive_prompt;
use crate::tui::agent::screen::VtCell;
use crate::tui::agent::ScreenSnapshot;
use crate::tui::app::types::App;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub fn render_vt_screen(frame: &mut Frame, area: Rect, snap: &ScreenSnapshot) {
    render_vt_screen_with_mask(frame, area, snap, false);
}

/// Determine which rows should have their input content masked.
/// When masking is active (sensitive input detected), we mask from the cursor
/// row upward through all rows that are part of the same wrapped prompt.
/// We stop going up when we hit an empty row or a row that already looks
/// like a sensitive prompt (the prompt text itself is kept visible).
fn compute_mask_rows(snap: &ScreenSnapshot) -> std::collections::HashSet<u16> {
    let mut mask_rows = std::collections::HashSet::new();
    let cursor = snap.cursor_row;
    let total_rows = snap.cells.len() as u16;

    if cursor >= total_rows {
        return mask_rows;
    }

    // The cursor row itself should always be masked (user input area)
    mask_rows.insert(cursor);

    // Walk upward from the row just above the cursor to find wrapped continuations.
    // A row is a continuation if it has non-space content and is NOT itself a
    // sensitive prompt line (that's the prompt text — we want to keep it visible).
    let mut row = cursor.saturating_sub(1);
    loop {
        if row >= total_rows {
            break;
        }
        let line_text = row_text(&snap.cells[row as usize]);
        let trimmed = line_text.trim();
        if trimmed.is_empty() {
            // Empty row = boundary, stop ascending
            break;
        }
        // If this row itself contains the sensitive prompt keyword, it's the
        // prompt text — keep it visible, don't mask it. But any rows between
        // it and the cursor row that are part of the user's input should be masked.
        if line_looks_sensitive_prompt(trimmed) {
            break;
        }
        // This is a wrapped continuation of the input — mask it
        mask_rows.insert(row);
        if row == 0 {
            break;
        }
        row -= 1;
    }

    mask_rows
}

fn row_text(row: &[Option<VtCell>]) -> String {
    let mut text = String::new();
    for c in row.iter().flatten() {
        if !c.ch.is_empty() {
            text.push_str(&c.ch);
        }
    }
    text
}

pub(super) fn render_vt_screen_with_mask(
    frame: &mut Frame,
    area: Rect,
    snap: &ScreenSnapshot,
    mask_cursor_line: bool,
) {
    let mask_rows = if mask_cursor_line {
        compute_mask_rows(snap)
    } else {
        std::collections::HashSet::new()
    };

    let buf = frame.buffer_mut();
    for (row_idx, row) in snap.cells.iter().enumerate() {
        if row_idx as u16 >= area.height {
            break;
        }
        let y = area.y + row_idx as u16;
        let is_masked_row = mask_rows.contains(&(row_idx as u16));

        for (col_idx, cell) in row.iter().enumerate() {
            if col_idx as u16 >= area.width {
                break;
            }
            let x = area.x + col_idx as u16;

            let Some(c) = cell else {
                continue;
            };

            let ch = if is_masked_row && !c.ch.is_empty() && c.ch != " " {
                // On the cursor row, only mask at/after cursor column to
                // preserve the prompt portion. On wrapped continuation rows
                // above, mask everything (those are the user's wrapped input).
                if row_idx as u16 == snap.cursor_row && (col_idx as u16) < snap.cursor_col {
                    &c.ch
                } else {
                    "•"
                }
            } else if c.ch.is_empty() {
                " "
            } else {
                &c.ch
            };
            let (fg, bg) = if c.inverse {
                (c.bg, c.fg)
            } else {
                (c.fg, c.bg)
            };

            let mut style = Style::default().fg(fg).bg(bg);
            if c.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if c.underline {
                style = style.add_modifier(Modifier::UNDERLINED);
            }

            let buf_cell = &mut buf[(x, y)];
            buf_cell.set_symbol(ch);
            buf_cell.set_style(style);
        }
    }
}

pub(super) fn render_indicators(frame: &mut Frame, inner: Rect, snap: &ScreenSnapshot, _app: &App) {
    if snap.scrolled {
        let msg = " \u{2592} SCROLLED \u{2592} "; // ▒ SCROLLED ▒
        let w = msg.chars().count() as u16; // display width (char count, not bytes)
        let x = inner.x + inner.width.saturating_sub(w + 1);
        let area = Rect::new(x, inner.y, w, 1);
        let widget = Paragraph::new(msg).style(Style::default().fg(Color::Yellow).bg(Color::Black));
        frame.render_widget(widget, area);
    }
}
