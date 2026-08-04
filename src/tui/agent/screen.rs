use crate::tui::agent::sanitize::{
    command_after_shell_prompt, is_ui_line, sanitize_line, strip_borders,
};
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
    vt: &mut super::Vt,
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

/// Divider written after replayed scrollback so a restored terminal can't be
/// mistaken for a fresh one, or for the boundary where live output resumes.
/// Dim SGR (`\x1b[2m`)/reset so it reads as chrome, not as something the
/// previous session printed.
const HISTORY_REPLAY_MARKER: &[u8] = b"\x1b[2m--- restored session history above ---\x1b[0m\r\n";

impl InteractiveAgent {
    /// Replay persisted plain-text scrollback into the VT100 parser.
    ///
    /// This reconstructs terminal history after session resume by feeding each
    /// line as terminal output with CRLF separators, followed by a dim marker
    /// line so the operator can see where the replay ends and live output
    /// begins.
    pub fn replay_scrollback_lines(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }

        if let Ok(mut vt) = self.vt.lock() {
            let mut replay = Vec::new();
            for line in lines {
                replay.extend_from_slice(line.as_bytes());
                replay.extend_from_slice(b"\r\n");
            }
            replay.extend_from_slice(HISTORY_REPLAY_MARKER);
            vt.process(&replay);
        }
        // Do NOT update last_output_at here. Replay is a history reconstruction
        // (auto-resume, new-terminal scrollback), not fresh PTY output. Stamping
        // now would make every replayed agent appear "actively working" for the
        // next ACTIVITY_IDLE_THRESHOLD_MS, which is the root cause of the
        // "all-green on navigation" bug — the activity timestamp must only move
        // when the PTY background reader (mod.rs) actually receives bytes.
    }

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
                    wide_continuation: c.is_wide_continuation(),
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

    /// Return the text of the *current command* — what the user is typing after
    /// the active shell/program prompt — so sensitive-prompt detection only
    /// looks at the line in progress, never at earlier output.
    ///
    /// If the cursor line already carries a shell-prompt marker (e.g.
    /// `user@host:~$ `), the command is just the text after it; a fresh empty
    /// prompt yields an empty string and never matches. Otherwise the line is
    /// program output (e.g. a `Vault passphrase:` prompt that wrapped on a
    /// narrow terminal), so we walk up to 5 rows joining the continuation,
    /// stopping at the shell prompt that launched it or at a blank row.
    pub(crate) fn prompt_context_text(&self) -> Option<String> {
        let vt = self.vt.try_lock().ok()?;
        let screen = vt.screen();
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 {
            return None;
        }

        let cursor = screen.cursor_position().0.min(rows.saturating_sub(1));
        let cursor_line = read_screen_line(screen, cursor, cols)?;
        let mut combined = sanitize_line(&cursor_line).trim_end().to_string();

        // A marker on the cursor line bounds the current command: anything
        // before it (including a stale `… wrong passphrase` error) is not part
        // of what's being typed now, so return only the post-marker text.
        if let Some(command) = command_after_shell_prompt(&combined) {
            return Some(command);
        }

        let mut row = cursor;
        let mut walked = 0u16;
        while row > 0 && walked < 5 {
            row -= 1;
            walked += 1;
            let Some(line_text) = read_screen_line(screen, row, cols) else {
                break;
            };
            let trimmed = sanitize_line(&line_text).trim_end().to_string();
            if trimmed.trim().is_empty() {
                break;
            }
            // Reaching the shell prompt that launched this program means we've
            // collected the whole wrapped prompt; earlier rows are history.
            if command_after_shell_prompt(&trimmed).is_some() {
                break;
            }
            combined = format!("{} {}", trimmed, combined);
        }

        Some(combined)
    }

    pub fn visible_text(&self) -> String {
        self.get_plain_text_from_screen().unwrap_or_default()
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

impl ScreenSnapshot {
    /// Extract the text covered by a linear selection from `start` to `end`
    /// (inclusive, `(row, col)` cells in reading order). Rows between the
    /// endpoints are taken whole; trailing whitespace is trimmed per line.
    pub fn selection_text(&self, start: (u16, u16), end: (u16, u16)) -> String {
        let (start, end) = if end < start {
            (end, start)
        } else {
            (start, end)
        };
        let mut lines = Vec::new();
        for row in start.0..=end.0 {
            let Some(cells) = self.cells.get(row as usize) else {
                break;
            };
            let from = if row == start.0 { start.1 as usize } else { 0 };
            let to = if row == end.0 {
                (end.1 as usize + 1).min(cells.len())
            } else {
                cells.len()
            };
            let mut line = String::new();
            for cell in cells.iter().take(to).skip(from) {
                match cell {
                    // The leading half of a wide char already contributed the
                    // full grapheme; its continuation cell adds nothing.
                    Some(c) if c.wide_continuation => {}
                    Some(c) if c.ch.is_empty() => line.push(' '),
                    Some(c) => line.push_str(&c.ch),
                    None => line.push(' '),
                }
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }
}

/// A single cell from the virtual terminal.
pub struct VtCell {
    pub ch: String,
    pub fg: ratatui::style::Color,
    pub bg: ratatui::style::Color,
    pub bold: bool,
    pub underline: bool,
    pub inverse: bool,
    /// Trailing half of a double-width character (contributes no text).
    pub wide_continuation: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(ch: &str) -> Option<VtCell> {
        Some(VtCell {
            ch: ch.to_string(),
            fg: ratatui::style::Color::Reset,
            bg: ratatui::style::Color::Reset,
            bold: false,
            underline: false,
            inverse: false,
            wide_continuation: false,
        })
    }

    fn row_from(text: &str, width: usize) -> Vec<Option<VtCell>> {
        let mut row: Vec<Option<VtCell>> = text.chars().map(|c| cell(&c.to_string())).collect();
        row.resize_with(width, || cell(""));
        row
    }

    fn snapshot(rows: &[&str], width: usize) -> ScreenSnapshot {
        ScreenSnapshot {
            cells: rows.iter().map(|r| row_from(r, width)).collect(),
            cursor_row: 0,
            cursor_col: 0,
            scrolled: false,
        }
    }

    #[test]
    fn selection_text_single_row_segment() {
        let snap = snapshot(&["hello world"], 20);
        assert_eq!(snap.selection_text((0, 6), (0, 10)), "world");
    }

    #[test]
    fn selection_text_multi_row_takes_full_middle_rows() {
        let snap = snapshot(&["first line", "middle", "last line"], 20);
        assert_eq!(snap.selection_text((0, 6), (2, 3)), "line\nmiddle\nlast");
    }

    #[test]
    fn selection_text_reversed_endpoints_and_blank_cells() {
        let snap = snapshot(&["a b", ""], 10);
        // Reversed (end before start) selects the same range; untouched cells
        // read as spaces and trailing whitespace is trimmed per line.
        assert_eq!(snap.selection_text((1, 5), (0, 0)), "a b\n");
    }

    #[test]
    fn selection_text_skips_wide_continuation_cells() {
        // "日" occupies two cells: the glyph plus a continuation cell.
        let mut row = vec![cell("日")];
        row.push(Some(VtCell {
            ch: String::new(),
            fg: ratatui::style::Color::Reset,
            bg: ratatui::style::Color::Reset,
            bold: false,
            underline: false,
            inverse: false,
            wide_continuation: true,
        }));
        row.push(cell("x"));
        row.resize_with(6, || cell(""));
        let snap = ScreenSnapshot {
            cells: vec![row],
            cursor_row: 0,
            cursor_col: 0,
            scrolled: false,
        };
        assert_eq!(snap.selection_text((0, 0), (0, 2)), "日x");
    }

    // ── from_vt100 color conversion ─────────────────────────────

    #[test]
    fn from_vt100_default_color() {
        assert_eq!(
            from_vt100(vt100::Color::Default),
            ratatui::style::Color::Reset
        );
    }

    #[test]
    fn from_vt100_indexed_color() {
        assert_eq!(
            from_vt100(vt100::Color::Idx(42)),
            ratatui::style::Color::Indexed(42)
        );
    }

    #[test]
    fn from_vt100_rgb_color() {
        assert_eq!(
            from_vt100(vt100::Color::Rgb(100, 200, 50)),
            ratatui::style::Color::Rgb(100, 200, 50)
        );
    }

    #[test]
    fn from_vt100_indexed_zero() {
        assert_eq!(
            from_vt100(vt100::Color::Idx(0)),
            ratatui::style::Color::Indexed(0)
        );
    }

    #[test]
    fn from_vt100_indexed_max() {
        assert_eq!(
            from_vt100(vt100::Color::Idx(255)),
            ratatui::style::Color::Indexed(255)
        );
    }

    // ── selection_text edge cases ────────────────────────────────

    #[test]
    fn selection_text_single_char() {
        let snap = snapshot(&["abc"], 10);
        assert_eq!(snap.selection_text((0, 1), (0, 1)), "b");
    }

    #[test]
    fn selection_text_full_row() {
        let snap = snapshot(&["hello"], 10);
        assert_eq!(snap.selection_text((0, 0), (0, 4)), "hello");
    }

    #[test]
    fn selection_text_empty_row() {
        let snap = snapshot(&[""], 10);
        let result = snap.selection_text((0, 0), (0, 0));
        assert!(result.is_empty() || result == " ");
    }

    #[test]
    fn selection_text_two_rows_no_overlap() {
        let snap = snapshot(&["aaa", "bbb"], 10);
        assert_eq!(snap.selection_text((0, 0), (1, 2)), "aaa\nbbb");
    }

    #[test]
    fn selection_text_none_cells_are_spaces() {
        let mut snap = snapshot(&["abc"], 5);
        // Set one cell to None
        snap.cells[0][1] = None;
        assert_eq!(snap.selection_text((0, 0), (0, 2)), "a c");
    }

    #[test]
    fn selection_text_reversed_endpoints() {
        let snap = snapshot(&["hello"], 10);
        // Reversed: start > end
        assert_eq!(snap.selection_text((0, 4), (0, 0)), "hello");
    }

    #[test]
    #[allow(clippy::vec_init_then_push)]
    fn selection_text_wide_char_with_continuation() {
        let mut row: Vec<Option<VtCell>> = Vec::new();
        // "AB" as two normal chars
        row.push(cell("A"));
        row.push(cell("B"));
        // "日" as wide char + continuation
        row.push(cell("日"));
        row.push(Some(VtCell {
            ch: String::new(),
            fg: ratatui::style::Color::Reset,
            bg: ratatui::style::Color::Reset,
            bold: false,
            underline: false,
            inverse: false,
            wide_continuation: true,
        }));
        row.push(cell("C"));
        row.resize_with(10, || cell(""));
        let snap = ScreenSnapshot {
            cells: vec![row],
            cursor_row: 0,
            cursor_col: 0,
            scrolled: false,
        };
        assert_eq!(snap.selection_text((0, 0), (0, 4)), "AB日C");
    }

    #[test]
    fn selection_text_empty_ch_cell() {
        let mut snap = snapshot(&["abc"], 5);
        // Set one cell to have empty ch
        snap.cells[0][1] = Some(VtCell {
            ch: String::new(),
            fg: ratatui::style::Color::Reset,
            bg: ratatui::style::Color::Reset,
            bold: false,
            underline: false,
            inverse: false,
            wide_continuation: false,
        });
        assert_eq!(snap.selection_text((0, 0), (0, 2)), "a c");
    }

    // ── replay_scrollback_lines ─────────────────────────────────

    /// `cat` is a lightweight stand-in shell; these tests only exercise the
    /// vt100 replay path and never depend on what `cat` does with stdin.
    fn spawn_test_terminal(name: &str) -> InteractiveAgent {
        InteractiveAgent::spawn_terminal(
            "cat",
            "/tmp",
            80,
            24,
            Some(name),
            &[],
            ratatui::style::Color::White,
        )
        .expect("spawn terminal")
    }

    #[test]
    fn replay_scrollback_lines_reproduces_the_given_lines() {
        let agent = spawn_test_terminal("replay-basic");
        agent.replay_scrollback_lines(&["old line one".to_string(), "old line two".to_string()]);
        let out = agent.last_lines(50);
        assert!(out.contains("old line one"));
        assert!(out.contains("old line two"));
    }

    #[test]
    fn replay_scrollback_lines_marks_replayed_content_as_history() {
        let agent = spawn_test_terminal("replay-marker");
        agent.replay_scrollback_lines(&["previous session output".to_string()]);
        let out = agent.last_lines(50);
        assert!(
            out.contains("restored session history above"),
            "restored content must be visually marked as history: {out:?}"
        );
    }

    #[test]
    fn replay_scrollback_lines_empty_is_a_noop() {
        let agent = spawn_test_terminal("replay-empty");
        agent.replay_scrollback_lines(&[]);
        let out = agent.last_lines(50);
        assert!(
            out.trim().is_empty(),
            "replaying no lines must not print a marker or anything else: {out:?}"
        );
    }
}
