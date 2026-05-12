use crate::tui::agent::sanitize::{is_ui_line, sanitize_line, strip_borders};
use crate::tui::agent::InteractiveAgent;

/// Read a single line from the vt100 screen at `row`, with panic protection.
fn read_screen_line(screen: &vt100::Screen, row: u16, cols: u16) -> Option<String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut line = String::with_capacity(cols as usize);
        for c in 0..cols {
            if let Some(cell) = screen.cell(row, c) {
                line.push_str(cell.contents());
            }
        }
        line
    }))
    .ok()
}

/// Read absolute buffer lines [from_abs, to_abs) from a vt100 parser.
///
/// `set_scrollback(S)` shows the window at absolute positions
/// `[max_sb - S .. max_sb - S + rows - 1]`.  Stepping down from a high
/// offset (oldest) to 0 (current screen) in increments of `rows` gives
/// non-overlapping pages.  `next_expected` ensures each absolute index is
/// emitted exactly once even when the final clamped page overlaps with the
/// previous one.
fn read_abs_range(
    vt: &mut vt100::Parser,
    max_sb: usize,
    rows: usize,
    from_abs: usize,
    to_abs: usize,
) -> Vec<String> {
    if to_abs <= from_abs || rows == 0 {
        return Vec::new();
    }

    // Find the page-aligned scrollback offset that first covers `from_abs`.
    // set_scrollback(S) starts at abs = max_sb - S.
    // We need max_sb - S <= from_abs  =>  S >= max_sb - from_abs.
    // Round up to the nearest multiple of `rows`, capped at max_sb.
    let s_for_from = max_sb.saturating_sub(from_abs);
    let s_start = if s_for_from.is_multiple_of(rows) {
        s_for_from
    } else {
        ((s_for_from / rows) + 1) * rows
    }
    .min(max_sb);

    let mut collected: Vec<String> = Vec::with_capacity(to_abs.saturating_sub(from_abs));
    let mut next_expected = from_abs;
    let mut s = s_start;

    loop {
        let clamped = s.min(max_sb);
        let page_start_abs = max_sb - clamped;
        vt.screen_mut().set_scrollback(clamped);
        let content = vt.screen().contents();

        for (i, line) in content.lines().enumerate() {
            let abs_idx = page_start_abs + i;
            if abs_idx == next_expected && abs_idx < to_abs {
                // Always advance the index — filtering only controls
                // whether the line is included in output, not whether
                // subsequent lines are reachable.
                next_expected += 1;
                let sanitized = sanitize_line(line).trim_end().to_string();
                if !sanitized.trim().is_empty() && !is_ui_line(&sanitized) {
                    // Strip box-drawing borders from response lines (TUI agents
                    // render output inside │ borders).
                    let cleaned = strip_borders(&sanitized);
                    if !cleaned.is_empty() {
                        collected.push(cleaned.to_string());
                    } else {
                        collected.push(sanitized);
                    }
                }
            }
        }

        if next_expected >= to_abs || s == 0 {
            break;
        }
        s = s.saturating_sub(rows);
    }

    collected
}

impl InteractiveAgent {
    /// Get a snapshot of the virtual terminal screen for rendering.
    ///
    /// Uses vt100's native scrollback: `set_scrollback(N)` shifts the
    /// viewport N rows up into history.  `cell()` then returns the
    /// visible (possibly scrolled) content with full colors.
    pub fn screen_snapshot(&self) -> Option<ScreenSnapshot> {
        let mut vt = self.vt.lock().ok()?;
        vt.screen_mut().set_scrollback(self.scroll_offset);

        let screen = vt.screen();
        let (rows, cols) = screen.size();

        let mut cells = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut row_cells = Vec::with_capacity(cols as usize);
            for col in 0..cols {
                row_cells.push(screen.cell(row, col).map(|c| VtCell {
                    ch: c.contents().to_string(),
                    fg: from_vt100(c.fgcolor()),
                    bg: from_vt100(c.bgcolor()),
                    bold: c.bold(),
                    underline: c.underline(),
                    inverse: c.inverse(),
                }));
            }
            cells.push(row_cells);
        }

        let cursor = screen.cursor_position();
        let scrolled = self.scroll_offset > 0;

        Some(ScreenSnapshot {
            cells,
            cursor_row: if scrolled { rows } else { cursor.0 },
            cursor_col: cursor.1,
            scrolled,
        })
    }

    /// Get a plain-text preview of the screen (for sidebar log preview).
    pub fn output(&self) -> String {
        if let Ok(vt) = self.vt.lock() {
            vt.screen().contents()
        } else {
            String::new()
        }
    }

    /// Get the last N lines of the entire history (scrollback + visible screen).
    pub fn last_lines(&self, n: usize) -> String {
        if n == 0 {
            return String::new();
        }
        let Ok(mut vt) = self.vt.lock() else {
            return String::new();
        };
        let (rows, _) = vt.screen().size();
        let rows = rows as usize;
        if rows == 0 {
            return String::new();
        }
        let prev_sb = vt.screen().scrollback();
        vt.screen_mut().set_scrollback(usize::MAX);
        let max_sb = vt.screen().scrollback();
        let total_lines = max_sb + rows;
        let from_abs = total_lines.saturating_sub(n);
        let result = read_abs_range(&mut vt, max_sb, rows, from_abs, total_lines);
        vt.screen_mut().set_scrollback(prev_sb);
        result.join("\n")
    }

    /// Extract lines at absolute buffer positions [from_abs, to_abs).
    ///
    /// `from_abs` and `to_abs` are the scrollback-history-depth values captured
    /// via `record_prompt` (i.e. the result of `set_scrollback(usize::MAX)` at
    /// the time of capture, not the current scroll offset).
    pub fn lines_at_scrollback_range(&self, from_abs: usize, to_abs: usize) -> String {
        if to_abs <= from_abs {
            return String::new();
        }
        let Ok(mut vt) = self.vt.lock() else {
            return String::new();
        };
        let (rows, _) = vt.screen().size();
        let rows = rows as usize;
        if rows == 0 {
            return String::new();
        }
        let prev_sb = vt.screen().scrollback();
        vt.screen_mut().set_scrollback(usize::MAX);
        let max_sb = vt.screen().scrollback();
        let result = read_abs_range(&mut vt, max_sb, rows, from_abs, to_abs);
        vt.screen_mut().set_scrollback(prev_sb);
        result.join("\n")
    }

    /// Extract the last N non-empty lines from the PTY output.
    /// Useful for capturing error messages from agents that exit immediately.
    pub fn last_output_lines(&self, n: usize) -> Vec<String> {
        let Ok(parser) = self.vt.lock() else {
            return Vec::new();
        };
        let screen = parser.screen();
        let rows = screen.size().0;
        let mut lines: Vec<String> = Vec::new();
        for row in 0..rows {
            let line = screen
                .rows_formatted(row, row + 1)
                .next()
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&line).trim().to_string();
            if !text.is_empty() {
                lines.push(text);
            }
        }
        // Also check scrollback
        let scrollback = screen.scrollback();
        if scrollback > 0 {
            let saved = parser.screen().rows_formatted(0, 0);
            for line in saved {
                let text = String::from_utf8_lossy(&line).trim().to_string();
                if !text.is_empty() {
                    lines.push(text);
                }
            }
        }
        lines
            .into_iter()
            .rev()
            .take(n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub(crate) fn current_visible_line_text(&self) -> Option<String> {
        let vt = self.vt.try_lock().ok()?;
        let screen = vt.screen();
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 {
            return None;
        }

        let row = screen.cursor_position().0.min(rows.saturating_sub(1));
        let mut line = String::new();
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }

        Some(sanitize_line(&line).trim_end().to_string())
    }

    /// Get plain text from the current visible screen area.
    /// This is used for copying clean text without ANSI formatting.
    pub fn get_plain_text_from_screen(&self) -> Option<String> {
        let vt = self.vt.try_lock().ok()?;
        let screen = vt.screen();
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 {
            return None;
        }

        let mut text = String::new();
        // Get text from all visible lines
        for row in 0..rows {
            let mut line = String::new();
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col) {
                    line.push_str(cell.contents());
                }
            }
            // Add line to text, preserving newlines
            if !line.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&sanitize_line(&line));
            }
        }

        Some(text)
    }

    /// Get plain text from a specific selection area.
    /// Used when user selects text with mouse.
    #[allow(dead_code)]
    pub fn get_plain_text_from_selection(
        &self,
        start_row: usize,
        end_row: usize,
    ) -> Option<String> {
        let vt = self.vt.try_lock().ok()?;
        let screen = vt.screen();
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 {
            return None;
        }

        let mut text = String::new();
        let start_row = start_row.min(rows.saturating_sub(1) as usize);
        let end_row = end_row.min(rows.saturating_sub(1) as usize);

        for row in start_row..=end_row {
            let mut line = String::new();
            for col in 0..cols {
                if let Some(cell) = screen.cell(row as u16, col) {
                    line.push_str(cell.contents());
                }
            }
            if !line.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&sanitize_line(&line));
            }
        }

        Some(text)
    }

    /// Get plain text from the line at a specific screen position.
    #[allow(dead_code)]
    pub fn get_line_text_at_position(&self, col: u16, row: u16) -> Option<String> {
        let vt = self.vt.try_lock().ok()?;
        let screen = vt.screen();
        let (screen_rows, screen_cols) = screen.size();
        if screen_rows == 0 || screen_cols == 0 {
            return None;
        }
        let actual_row = if self.in_alternate_screen() {
            row.saturating_add(self.scroll_offset as u16)
        } else {
            row
        };
        if actual_row >= screen_rows || col >= screen_cols {
            return None;
        }
        let line = read_screen_line(screen, actual_row, screen_cols)?;
        let sanitized = sanitize_line(&line);
        if sanitized.trim().is_empty() {
            None
        } else {
            Some(sanitized)
        }
    }

    /// Get plain text from the current cursor line.
    #[allow(dead_code)]
    pub fn get_current_line_text(&self) -> Option<String> {
        let vt = self.vt.try_lock().ok()?;
        let screen = vt.screen();
        let (screen_rows, screen_cols) = screen.size();
        if screen_rows == 0 || screen_cols == 0 {
            return None;
        }
        let cursor_row = screen.cursor_position().0;
        let actual_row = if self.in_alternate_screen() {
            cursor_row.saturating_add(self.scroll_offset as u16)
        } else {
            cursor_row
        };
        if actual_row >= screen_rows {
            return None;
        }
        let line = read_screen_line(screen, actual_row, screen_cols)?;
        let sanitized = sanitize_line(&line);
        if sanitized.trim().is_empty() {
            None
        } else {
            Some(sanitized)
        }
    }

    /// Maximum scroll offset — try setting a large value and read back
    /// the clamped result from vt100's scrollback.
    pub fn max_scroll(&self) -> usize {
        if let Ok(mut vt) = self.vt.lock() {
            let prev = vt.screen().scrollback();
            vt.screen_mut().set_scrollback(usize::MAX);
            let max = vt.screen().scrollback();
            vt.screen_mut().set_scrollback(prev);
            max
        } else {
            0
        }
    }

    /// Total lines available: scrollback history + visible screen rows.
    /// Use this as the upper bound for context capture so that content
    /// currently on screen (not yet scrolled into history) is included.
    pub fn total_depth(&self) -> usize {
        if let Ok(mut vt) = self.vt.lock() {
            let prev = vt.screen().scrollback();
            vt.screen_mut().set_scrollback(usize::MAX);
            let max_sb = vt.screen().scrollback();
            let (rows, _) = vt.screen().size();
            vt.screen_mut().set_scrollback(prev);
            max_sb + rows as usize
        } else {
            0
        }
    }
}

/// A snapshot of the virtual terminal screen.
pub struct ScreenSnapshot {
    pub cells: Vec<Vec<Option<VtCell>>>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub scrolled: bool,
}

/// A single cell from the virtual terminal.
pub struct VtCell {
    pub ch: String,
    pub fg: ratatui::style::Color,
    pub bg: ratatui::style::Color,
    pub bold: bool,
    pub underline: bool,
    pub inverse: bool,
}
/// Convert vt100 color to ratatui color.
///
/// Passes through indexed colors (0-255) and truecolor RGB unchanged,
/// preserving each agent's original color scheme.
fn from_vt100(color: vt100::Color) -> ratatui::style::Color {
    use ratatui::style::Color;
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
