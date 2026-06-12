use crate::tui::agent::sanitize::{line_looks_sensitive_prompt, strip_shell_prompt_prefix};
use crate::tui::agent::{AgentStatus, InteractiveAgent, PromptEntry, MAX_PROMPT_HISTORY};
use anyhow::Result;
use chrono::Utc;
use std::time::Duration;

impl InteractiveAgent {
    /// Record a user prompt submission. Called when Enter is pressed.
    /// Captures the input and the current scrollback depth as the start
    /// of the response range (visible screen content starts at max_sb).
    pub fn record_prompt(&self, input: &str) {
        // Use scrollback depth only — visible screen lines are indexed from max_sb
        // upward, so this is the correct start for the response range.
        let history_depth = if let Ok(mut vt) = self.vt.lock() {
            let prev = vt.screen().scrollback();
            vt.screen_mut().set_scrollback(usize::MAX);
            let depth = vt.screen().scrollback();
            vt.screen_mut().set_scrollback(prev);
            depth
        } else {
            0
        };

        if let Ok(mut history) = self.prompt_history.lock() {
            // Close out the previous entry's response range using total_depth
            // so that visible screen lines (not yet in scrollback) are included.
            if let Some(last) = history.back_mut() {
                last.output_range.1 = history_depth + {
                    // Re-lock vt to get rows for total depth
                    if let Ok(vt) = self.vt.lock() {
                        vt.screen().size().0 as usize
                    } else {
                        0
                    }
                };
            }
            history.push_back(PromptEntry {
                input: input.to_string(),
                output_range: (history_depth, history_depth),
                timestamp: Utc::now(),
            });
            while history.len() > MAX_PROMPT_HISTORY {
                history.pop_front();
            }
        }
    }

    /// Detect if the agent appears to be waiting for user input / confirmation.
    ///
    /// Conservative strategy (cursor-position only — no text-pattern matching):
    /// 1. Process is running AND idle for 5+ seconds with new output.
    /// 2. Cursor is on the very last row (not middle) of the visible screen.
    ///
    /// Thresholds are deliberately high to avoid false positives. Once activated,
    /// will not reactivate unless there's new output (prevents flashing on idle).
    pub fn is_waiting_for_input(&self) -> bool {
        if self.status != AgentStatus::Running {
            return false;
        }

        let (is_idle, new_output) = self.check_idle_state();
        // Only show waiting if idle AND we saw new output (prevents re-triggering on no change)
        if !is_idle || !new_output {
            return false;
        }

        let Some(screen) = self.screen_snapshot() else {
            return false;
        };
        let rows = screen.cells.len();
        if rows == 0 {
            return false;
        }

        let last_nonempty = (0..rows).rev().find(|&r| {
            screen.cells.get(r).is_some_and(|row| {
                row.iter()
                    .any(|c| c.as_ref().is_some_and(|cell| !cell.ch.trim().is_empty()))
            })
        });

        let Some(last_row) = last_nonempty else {
            return false;
        };
        // Cursor must be on the actual last non-empty row (very last, not just lower half)
        // and be near the right side to indicate prompt awaiting input
        is_idle
            && (screen.cursor_row as usize) == last_row
            && screen.cursor_col as usize > last_row.saturating_sub(5)
    }

    fn check_idle_state(&self) -> (bool, bool) {
        const IDLE_MS: i64 = 5000; // 5 seconds — much more conservative
        let Ok(out) = self.last_output_at.lock() else {
            return (false, false);
        };
        let Ok(view) = self.last_viewed_at.lock() else {
            return (false, false);
        };
        let elapsed = Utc::now().signed_duration_since(*out).num_milliseconds();
        (elapsed >= IDLE_MS, *out > *view)
    }

    pub fn is_sensitive_input_active(&self) -> bool {
        self.prompt_context_text()
            .is_some_and(|text| line_looks_sensitive_prompt(&text))
    }

    pub fn should_bypass_warp_input(&self) -> bool {
        self.in_alternate_screen() || self.is_sensitive_input_active()
    }

    pub fn sync_warp_input_from_pty(&self, wait: Duration) -> Option<String> {
        if wait > Duration::ZERO {
            std::thread::sleep(wait);
        }

        self.current_visible_line_text()
            .map(|line| strip_shell_prompt_prefix(&line))
    }

    /// Whether the child process is using alternate screen mode.
    pub fn in_alternate_screen(&self) -> bool {
        self.vt
            .try_lock()
            .map(|vt| vt.screen().alternate_screen())
            .unwrap_or(false)
    }

    /// Forward a mouse event to the PTY.
    ///
    /// Checks the child's mouse protocol mode. If mouse reporting is
    /// active, sends the event in the correct encoding (SGR or X10).
    /// Returns `true` if the event was forwarded, `false` if no mouse
    /// protocol is active (caller should handle the event internally).
    pub fn forward_mouse(
        &self,
        kind: ratatui::crossterm::event::MouseEventKind,
        button: ratatui::crossterm::event::MouseButton,
        col: u16,
        row: u16,
    ) -> Result<bool> {
        let (mode, encoding, _cols) = {
            let vt = self.vt.lock().map_err(|_| anyhow::anyhow!("vt lock"))?;
            let s = vt.screen();
            (
                s.mouse_protocol_mode(),
                s.mouse_protocol_encoding(),
                s.size().1,
            )
        };

        use vt100::MouseProtocolEncoding as MPE;
        use vt100::MouseProtocolMode as MPM;

        match mode {
            MPM::None => Ok(false),
            _ => {
                let (btn_code, is_release) = match kind {
                    ratatui::crossterm::event::MouseEventKind::Down(
                        ratatui::crossterm::event::MouseButton::Left,
                    ) => (0u8, false),
                    ratatui::crossterm::event::MouseEventKind::Down(
                        ratatui::crossterm::event::MouseButton::Middle,
                    ) => (1, false),
                    ratatui::crossterm::event::MouseEventKind::Down(
                        ratatui::crossterm::event::MouseButton::Right,
                    ) => (2, false),
                    ratatui::crossterm::event::MouseEventKind::Up(_) => (3, true),
                    ratatui::crossterm::event::MouseEventKind::Drag(_) => {
                        let base = match button {
                            ratatui::crossterm::event::MouseButton::Left => 0,
                            ratatui::crossterm::event::MouseButton::Middle => 1,
                            ratatui::crossterm::event::MouseButton::Right => 2,
                        };
                        (base + 32, false)
                    }
                    ratatui::crossterm::event::MouseEventKind::Moved => {
                        return Ok(false);
                    }
                    ratatui::crossterm::event::MouseEventKind::ScrollUp => (64, false),
                    ratatui::crossterm::event::MouseEventKind::ScrollDown => (65, false),
                    _ => return Ok(false),
                };

                let x = col + 1;
                let y = row + 1;

                let seq = match encoding {
                    MPE::Sgr => {
                        let term = if is_release { 'm' } else { 'M' };
                        format!("\x1b[<{};{};{}{}", btn_code, x, y, term).into_bytes()
                    }
                    _ => {
                        if is_release {
                            vec![
                                0x1b,
                                b'[',
                                b'M',
                                (3 + 32) as u8,
                                (x as u8).saturating_add(32),
                                (y as u8).saturating_add(32),
                            ]
                        } else {
                            vec![
                                0x1b,
                                b'[',
                                b'M',
                                btn_code.wrapping_add(32),
                                (x as u8).saturating_add(32),
                                (y as u8).saturating_add(32),
                            ]
                        }
                    }
                };

                self.write_to_pty(&seq)?;
                Ok(true)
            }
        }
    }

    /// Forward a mouse scroll event to the PTY.
    ///
    /// Checks the child's mouse protocol mode.  If mouse reporting is
    /// active, sends the wheel event in the correct encoding.  Otherwise
    /// falls back to arrow-key sequences.
    pub fn forward_scroll(&self, scroll_up: bool) -> Result<()> {
        let (mode, encoding, cols) = {
            let vt = self.vt.lock().map_err(|_| anyhow::anyhow!("vt lock"))?;
            let s = vt.screen();
            (
                s.mouse_protocol_mode(),
                s.mouse_protocol_encoding(),
                s.size().1,
            )
        };

        use vt100::MouseProtocolEncoding as MPE;
        use vt100::MouseProtocolMode as MPM;

        match mode {
            MPM::None => {
                // No mouse protocol — send PgUp/PgDn (works in most TUIs)
                let seq: &[u8] = if scroll_up { b"\x1b[5~" } else { b"\x1b[6~" };
                self.write_to_pty(seq)
            }
            _ => {
                let button: u8 = if scroll_up { 64 } else { 65 };
                let col: u16 = cols / 2;
                let row: u16 = 10;
                let single = match encoding {
                    MPE::Sgr => format!("\x1b[<{};{};{}M", button, col + 1, row + 1).into_bytes(),
                    _ => {
                        vec![
                            0x1b,
                            b'[',
                            b'M',
                            button + 32,
                            (col as u8).wrapping_add(33),
                            (row as u8).wrapping_add(33),
                        ]
                    }
                };
                self.write_to_pty(&single)
            }
        }
    }
}
