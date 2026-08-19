use crate::domain::cli_config::PasteSubmitSpec;
use crate::tui::agent::sanitize::{line_looks_sensitive_prompt, strip_shell_prompt_prefix};
use crate::tui::agent::{
    AgentStatus, InteractiveAgent, PromptEntry, ACTIVITY_IDLE_THRESHOLD_MS, MAX_PROMPT_HISTORY,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::io::Write;
use std::time::Duration;

impl InteractiveAgent {
    /// True when the session has registered PTY output within
    /// [`ACTIVITY_IDLE_THRESHOLD_MS`] — i.e. it is actively working rather
    /// than merely alive. Used to drive the green/blue status color split
    /// for interactive sessions; distinct from [`Self::is_waiting_for_input`],
    /// which looks at cursor position to guess whether the agent wants a
    /// reply.
    pub fn has_recent_activity(&self) -> bool {
        let Ok(last_output_at) = self.last_output_at.lock() else {
            return false;
        };
        recently_active(*last_output_at, Utc::now())
    }
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

    /// True when the PTY's foreground process group differs from the spawned
    /// shell — an interactive child (wizard, editor, running command) owns the
    /// terminal, so input must go straight to the PTY instead of the warp buffer.
    pub fn foreground_app_active(&self) -> bool {
        let Some(shell_pid) = self.child.try_lock().ok().and_then(|c| c.process_id()) else {
            return false;
        };
        let Some(fg_group) = self
            .master
            .try_lock()
            .ok()
            .and_then(|m| m.process_group_leader())
        else {
            return false;
        };
        fg_group > 0 && fg_group as u32 != shell_pid
    }

    pub fn should_bypass_warp_input(&self) -> bool {
        self.in_alternate_screen() || self.is_sensitive_input_active() || {
            // Only shells host foreground children worth bypassing for; AI-CLI
            // sessions keep warp semantics untouched.
            self.is_terminal && self.foreground_app_active()
        }
    }

    /// Whether the child program switched the terminal into bracketed paste mode.
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.vt
            .try_lock()
            .map(|vt| vt.screen().bracketed_paste())
            .unwrap_or(false)
    }

    /// Paste text into the PTY, honoring the child's bracketed-paste mode.
    /// Programs that never enabled mode 2004 (simple prompts, wizards) would
    /// otherwise receive the literal `ESC[200~` markers as garbage input.
    pub fn paste_to_pty(&self, text: &str) -> Result<()> {
        if self.bracketed_paste_enabled() {
            let wrapped = format!("\x1b[200~{text}\x1b[201~");
            self.write_to_pty(wrapped.as_bytes())
        } else {
            self.write_to_pty(text.as_bytes())
        }
    }

    /// Deliver a prompt-builder prompt to the PTY as a SUBMITTED message —
    /// not text left pending in the target's input box. See
    /// [`write_submitted_prompt`] for why the submit keystroke must be a
    /// separate write from the paste.
    pub fn submit_prompt_to_pty(&self, prompt: &str, spec: PasteSubmitSpec) -> Result<()> {
        let bracketed = self.bracketed_paste_enabled();
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("pty writer lock poisoned"))?;
        write_submitted_prompt(&mut *writer, prompt, bracketed, spec)?;
        Ok(())
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
    ///
    /// Scroll ticks are special-cased: a full-screen child (alternate
    /// screen) that has no mouse protocol still needs *something*, since
    /// canopy has no scrollback of its own to fall back on for it (see
    /// `agents.rs`'s close-time capture comment for why). So scroll ticks
    /// forward through even under `MouseProtocolMode::None` once the child
    /// owns the screen, via the same [`encode_scroll_sequence`] that
    /// [`Self::forward_scroll`] uses — see [`should_forward_scroll_to_child`].
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

        use ratatui::crossterm::event::MouseEventKind;
        use vt100::MouseProtocolMode as MPM;

        if let Some(scroll_up) = scroll_direction(kind) {
            if !should_forward_scroll_to_child(mode, self.in_alternate_screen()) {
                return Ok(false);
            }
            let seq = encode_scroll_sequence(mode, encoding, scroll_up, col, row);
            self.write_to_pty(&seq)?;
            return Ok(true);
        }

        match mode {
            MPM::None => Ok(false),
            _ => {
                let (btn_code, is_release) = match kind {
                    MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
                        (0u8, false)
                    }
                    MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Middle) => {
                        (1, false)
                    }
                    MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Right) => {
                        (2, false)
                    }
                    MouseEventKind::Up(_) => (3, true),
                    MouseEventKind::Drag(_) => {
                        let base = match button {
                            ratatui::crossterm::event::MouseButton::Left => 0,
                            ratatui::crossterm::event::MouseButton::Middle => 1,
                            ratatui::crossterm::event::MouseButton::Right => 2,
                        };
                        (base + 32, false)
                    }
                    MouseEventKind::Moved => {
                        return Ok(false);
                    }
                    _ => return Ok(false),
                };

                let x = col + 1;
                let y = row + 1;

                let seq = match encoding {
                    vt100::MouseProtocolEncoding::Sgr => {
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
    /// Only called once the caller has already established the child owns
    /// the screen (alternate screen — see `scroll_terminal_like_agent`), so
    /// unlike [`Self::forward_mouse`] this always sends something: the
    /// wheel event in the child's mouse-protocol encoding if it has one
    /// active, otherwise the [`encode_scroll_sequence`] PgUp/PgDn fallback.
    /// Shares that encoder with `forward_mouse` so the two can't disagree
    /// about what a given `MouseProtocolMode` means.
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

        let col: u16 = cols / 2;
        let row: u16 = 10;
        let seq = encode_scroll_sequence(mode, encoding, scroll_up, col, row);
        self.write_to_pty(&seq)
    }
}

/// `Some(true)` for a scroll-up tick, `Some(false)` for scroll-down, `None`
/// for any other mouse event kind.
fn scroll_direction(kind: ratatui::crossterm::event::MouseEventKind) -> Option<bool> {
    match kind {
        ratatui::crossterm::event::MouseEventKind::ScrollUp => Some(true),
        ratatui::crossterm::event::MouseEventKind::ScrollDown => Some(false),
        _ => None,
    }
}

/// Whether a scroll tick should be forwarded to the child at all, versus
/// left for the caller to apply to canopy's own scrollback.
///
/// A child with an active mouse protocol always gets it (functional
/// requirement 2). A child with none only gets it once it owns the screen
/// (alternate screen) — that's the one case where canopy has no buffer of
/// its own to scroll instead (vt100's alternate grid has no scrollback; see
/// the close-time capture comment in `app/agents.rs`). Outside alternate
/// screen with no protocol, declining lets the caller scroll canopy's own
/// buffer, unchanged from today (functional requirement 3) — and that
/// caller-side scrolling is itself a real action, so a scroll tick is never
/// silently dropped either way (functional requirement 1).
fn should_forward_scroll_to_child(
    mode: vt100::MouseProtocolMode,
    in_alternate_screen: bool,
) -> bool {
    mode != vt100::MouseProtocolMode::None || in_alternate_screen
}

/// Encode a single scroll-wheel tick as PTY bytes for the child's current
/// mouse-protocol mode/encoding. Shared by [`InteractiveAgent::forward_mouse`]
/// and [`InteractiveAgent::forward_scroll`] so the two paths can never
/// disagree about what a given `MouseProtocolMode` means for a scroll tick.
fn encode_scroll_sequence(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    scroll_up: bool,
    col: u16,
    row: u16,
) -> Vec<u8> {
    use vt100::MouseProtocolEncoding as MPE;
    use vt100::MouseProtocolMode as MPM;

    match mode {
        MPM::None => {
            // No mouse protocol — PgUp/PgDn works in most full-screen TUIs.
            if scroll_up {
                b"\x1b[5~".to_vec()
            } else {
                b"\x1b[6~".to_vec()
            }
        }
        _ => {
            let button: u8 = if scroll_up { 64 } else { 65 };
            let x = col + 1;
            let y = row + 1;
            match encoding {
                MPE::Sgr => format!("\x1b[<{};{};{}M", button, x, y).into_bytes(),
                _ => vec![
                    0x1b,
                    b'[',
                    b'M',
                    button.wrapping_add(32),
                    (x as u8).saturating_add(32),
                    (y as u8).saturating_add(32),
                ],
            }
        }
    }
}

/// Pure predicate behind [`InteractiveAgent::has_recent_activity`]: was
/// `last_output_at` within [`ACTIVITY_IDLE_THRESHOLD_MS`] of `now`?
fn recently_active(last_output_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(last_output_at).num_milliseconds() < ACTIVITY_IDLE_THRESHOLD_MS
}

/// Write `prompt` to `writer` as a SUBMITTED message: the paste lands as one
/// block (bracketed only when the target actually enabled bracketed-paste
/// mode — see [`InteractiveAgent::bracketed_paste_enabled`]), then the
/// submit keystroke is written as a SEPARATE event after `spec.settle`.
///
/// This separation matters: writing the submit key immediately after the
/// paste (zero delay, same logical write) lets the two coalesce into one
/// chunk at the pty/kernel level. A target CLI with bracketed-paste
/// handling that reads a `\r` inside (or immediately trailing, same-chunk)
/// the paste event can fold it into the pasted text as a literal newline
/// instead of parsing it as a distinct Enter keypress — the prompt lands in
/// the input box but is never submitted.
pub fn write_submitted_prompt(
    writer: &mut impl Write,
    prompt: &str,
    bracketed: bool,
    spec: PasteSubmitSpec,
) -> std::io::Result<()> {
    if bracketed {
        writer.write_all(format!("\x1b[200~{prompt}\x1b[201~").as_bytes())?;
    } else {
        writer.write_all(prompt.as_bytes())?;
    }
    writer.flush()?;

    for _ in 0..spec.presses.max(1) {
        if spec.settle > Duration::ZERO {
            std::thread::sleep(spec.settle);
        }
        writer.write_all(spec.submit_key)?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod activity_tests {
    use super::recently_active;
    use chrono::{Duration, Utc};

    #[test]
    fn output_just_now_counts_as_active() {
        let now = Utc::now();
        assert!(recently_active(now, now));
    }

    #[test]
    fn output_inside_window_counts_as_active() {
        let now = Utc::now();
        let last_output_at = now - Duration::milliseconds(500);
        assert!(recently_active(last_output_at, now));
    }

    #[test]
    fn output_past_window_is_stale() {
        let now = Utc::now();
        let last_output_at = now - Duration::milliseconds(12_001);
        assert!(!recently_active(last_output_at, now));
    }
}

#[cfg(test)]
mod submit_sequencing_tests {
    use super::{write_submitted_prompt, PasteSubmitSpec};
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// Fake pty writer that records each `write_all` call as its own chunk,
    /// mirroring how a real pty preserves write-call boundaries at rest but
    /// lets a caller accidentally coalesce them by writing back-to-back with
    /// zero delay. Recording per-call lets tests assert the paste and the
    /// submit keystroke are genuinely separate writes, not just adjacent
    /// bytes in one buffer.
    #[derive(Default)]
    struct FakeWriter {
        chunks: Vec<Vec<u8>>,
    }

    impl Write for FakeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.chunks.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn instant_spec() -> PasteSubmitSpec {
        PasteSubmitSpec {
            settle: Duration::ZERO,
            submit_key: b"\r",
            presses: 1,
        }
    }

    #[test]
    fn paste_and_submit_are_separate_writes() {
        let mut writer = FakeWriter::default();
        write_submitted_prompt(&mut writer, "hello", true, instant_spec()).unwrap();

        assert_eq!(writer.chunks.len(), 2);
        assert_eq!(writer.chunks[0], b"\x1b[200~hello\x1b[201~".to_vec());
        assert_eq!(writer.chunks[1], b"\r".to_vec());
    }

    #[test]
    fn skips_bracket_markers_when_target_never_enabled_bracketed_paste() {
        let mut writer = FakeWriter::default();
        write_submitted_prompt(&mut writer, "hello", false, instant_spec()).unwrap();

        assert_eq!(writer.chunks[0], b"hello".to_vec());
    }

    #[test]
    fn honors_lf_submit_key_override() {
        let mut writer = FakeWriter::default();
        let spec = PasteSubmitSpec {
            settle: Duration::ZERO,
            submit_key: b"\n",
            presses: 1,
        };
        write_submitted_prompt(&mut writer, "hi", true, spec).unwrap();

        assert_eq!(writer.chunks[1], b"\n".to_vec());
    }

    #[test]
    fn sends_submit_key_once_per_configured_press() {
        let mut writer = FakeWriter::default();
        let spec = PasteSubmitSpec {
            settle: Duration::ZERO,
            submit_key: b"\r",
            presses: 2,
        };
        write_submitted_prompt(&mut writer, "hi", true, spec).unwrap();

        // One paste chunk + two separate submit-key writes.
        assert_eq!(writer.chunks.len(), 3);
        assert_eq!(writer.chunks[1], b"\r".to_vec());
        assert_eq!(writer.chunks[2], b"\r".to_vec());
    }

    #[test]
    fn settle_delay_elapses_before_the_submit_write() {
        let mut writer = FakeWriter::default();
        let spec = PasteSubmitSpec {
            settle: Duration::from_millis(20),
            submit_key: b"\r",
            presses: 1,
        };

        let start = Instant::now();
        write_submitted_prompt(&mut writer, "hi", true, spec).unwrap();

        assert!(start.elapsed() >= Duration::from_millis(20));
        // The delay must not stall the paste write itself — only the submit
        // keystroke after it.
        assert_eq!(writer.chunks[0], b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn zero_presses_still_sends_one_submit_key() {
        let mut writer = FakeWriter::default();
        let spec = PasteSubmitSpec {
            settle: Duration::ZERO,
            submit_key: b"\r",
            presses: 0,
        };
        write_submitted_prompt(&mut writer, "hi", true, spec).unwrap();

        assert_eq!(writer.chunks.len(), 2);
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::{encode_scroll_sequence, should_forward_scroll_to_child};
    use vt100::{MouseProtocolEncoding as MPE, MouseProtocolMode as MPM};

    // C6: `forward_mouse` and `forward_scroll` used to decide independently
    // what a scroll tick under a given `MouseProtocolMode` should do, and
    // could disagree. Both now delegate to `encode_scroll_sequence` for the
    // bytes and `should_forward_scroll_to_child` for whether to send them at
    // all — these tests pin that shared decision directly, since it's the
    // one thing both call sites must never drift apart on.

    #[test]
    fn no_protocol_and_no_alternate_screen_defers_to_canopy_scrollback() {
        // A plain shell with no mouse protocol: canopy has its own
        // scrollback here, so the child should not receive anything.
        assert!(!should_forward_scroll_to_child(MPM::None, false));
    }

    #[test]
    fn no_protocol_but_alternate_screen_forwards_to_child() {
        // A full-screen child (e.g. Codex) with no mouse protocol: canopy
        // has no scrollback of its own for the alternate grid, so the tick
        // must still go somewhere rather than vanish.
        assert!(should_forward_scroll_to_child(MPM::None, true));
    }

    #[test]
    fn active_protocol_always_forwards_regardless_of_alternate_screen() {
        assert!(should_forward_scroll_to_child(MPM::PressRelease, false));
        assert!(should_forward_scroll_to_child(MPM::PressRelease, true));
    }

    #[test]
    fn every_decision_results_in_some_action_not_nothing() {
        // Whatever the (mode, alternate_screen) combination, there is
        // always a concrete, observable outcome: forward to the child
        // (checked here) or defer to canopy's own buffer (the `false`
        // cases above, which the caller always honors — see
        // `scroll_terminal_like_agent` in event/mod.rs). No combination
        // produces a silent no-op.
        for mode in [MPM::None, MPM::PressRelease] {
            for alt_screen in [false, true] {
                let forwards = should_forward_scroll_to_child(mode, alt_screen);
                if forwards {
                    let seq = encode_scroll_sequence(mode, MPE::Sgr, true, 0, 0);
                    assert!(!seq.is_empty());
                }
                // else: the caller scrolls canopy's own buffer instead —
                // also a real action, just not a PTY write.
            }
        }
    }

    #[test]
    fn no_protocol_sends_pgup_pgdn_fallback() {
        assert_eq!(
            encode_scroll_sequence(MPM::None, MPE::Sgr, true, 5, 5),
            b"\x1b[5~".to_vec()
        );
        assert_eq!(
            encode_scroll_sequence(MPM::None, MPE::Sgr, false, 5, 5),
            b"\x1b[6~".to_vec()
        );
    }

    #[test]
    fn fallback_ignores_encoding_since_none_mode_has_no_wheel_protocol() {
        let via_sgr = encode_scroll_sequence(MPM::None, MPE::Sgr, true, 5, 5);
        let via_default = encode_scroll_sequence(MPM::None, MPE::Default, true, 5, 5);
        assert_eq!(via_sgr, via_default);
    }

    #[test]
    fn sgr_encoding_reports_button_64_for_scroll_up() {
        let seq = encode_scroll_sequence(MPM::PressRelease, MPE::Sgr, true, 9, 4);
        assert_eq!(seq, b"\x1b[<64;10;5M".to_vec());
    }

    #[test]
    fn sgr_encoding_reports_button_65_for_scroll_down() {
        let seq = encode_scroll_sequence(MPM::PressRelease, MPE::Sgr, false, 9, 4);
        assert_eq!(seq, b"\x1b[<65;10;5M".to_vec());
    }
}
