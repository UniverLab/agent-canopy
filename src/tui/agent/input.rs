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

    /// Whether the child has pushed Kitty keyboard protocol flags (`CSI >
    /// flags u`) at any point in the session. Tracked from a raw scan of
    /// PTY output — see [`parse_kitty_keyboard_push`] — independent of the
    /// vt100 parser, which has no support of its own for this protocol
    /// (vt100 0.16.2's `perform.rs` has no `u`-terminated CSI dispatch at
    /// all).
    pub fn kitty_keyboard_negotiated(&self) -> bool {
        self.kitty_keyboard_flags
            .try_lock()
            .map(|f| f.is_some())
            .unwrap_or(false)
    }

    /// Forward a mouse event to the PTY.
    ///
    /// Checks the child's mouse protocol mode. If mouse reporting is
    /// active, sends the event in the correct encoding (SGR or X10).
    /// Returns `true` if the event was forwarded, `false` if no mouse
    /// protocol is active (caller should handle the event internally).
    ///
    /// Scroll ticks are forwarded (and consumed) here only when the child
    /// has an active mouse protocol — see [`should_forward_scroll_to_child`].
    /// Under `MouseProtocolMode::None` this always declines and writes
    /// nothing, even inside a full-screen child's alternate screen: this
    /// method has no way to know the guessed fallback keystroke actually
    /// meant anything to the child, so consuming the event here would risk
    /// silently eating a scroll gesture the child never understood (see the
    /// C6b writeup — this exact bug shipped once already). The alternate
    /// screen without a mouse protocol case is instead handled by
    /// [`Self::forward_scroll`] via `scroll_terminal_like_agent` in
    /// `event/mod.rs`, which always runs as a fallback after this method
    /// declines, so the tick is never silently dropped either way.
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
            if !should_forward_scroll_to_child(mode) {
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

/// Whether [`InteractiveAgent::forward_mouse`] should forward a scroll tick
/// to the child *and treat it as consumed*.
///
/// True only when the child has an active mouse protocol — it explicitly
/// asked for wheel events, so canopy knows the bytes mean something to it.
/// Under `MouseProtocolMode::None` this is always false, even inside a
/// full-screen child's alternate screen: `forward_mouse` has no
/// confirmation the child understands the PgUp/PgDn fallback, so it must
/// not consume the event on that guess (that guess shipped once already —
/// see the C6b writeup — and made scrolling over Codex a dead gesture,
/// since forwarding *and* consuming meant canopy's own fallback scroll
/// never got a turn). The alternate-screen-without-a-protocol case is
/// handled separately by [`InteractiveAgent::forward_scroll`], invoked
/// unconditionally by `scroll_terminal_like_agent` in `event/mod.rs`
/// whenever `forward_mouse` declines — so the tick still reaches the child
/// there, just never at the cost of silently eating canopy's own fallback.
fn should_forward_scroll_to_child(mode: vt100::MouseProtocolMode) -> bool {
    mode != vt100::MouseProtocolMode::None
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
            // No mouse protocol — legacy PgUp/PgDn (`CSI 5 ~` / `CSI 6 ~`)
            // is sent unconditionally, whether or not the child pushed
            // Kitty keyboard protocol flags (see `kitty_keyboard_negotiated`
            // above). This is not a fallback we chose over a "real" Kitty
            // encoding — for an unmodified Page Up/Page Down keypress there
            // is no other encoding to send. The Kitty keyboard protocol's
            // own "Functional key codes" table (kovidgoyal/kitty
            // docs/keyboard-protocol.rst) lists PAGE_UP as `5 ~` and
            // PAGE_DOWN as `6 ~` — identical to the legacy table — because
            // these keys only gain a distinct `CSI number u` / PUA-codepoint
            // form under the "report all keys as escape codes" enhancement
            // bit (0x8), and even then the codepoints listed (57421/57422)
            // are for the *keypad* variants (KP_PAGE_UP/KP_PAGE_DOWN), not
            // the plain keys a scroll-wheel tick maps to. So `\x1b[5~` /
            // `\x1b[6~` already *is* "the Kitty encoding" here.
            //
            // A prior version of this comment claimed to have verified this
            // live against Codex by driving a "Transcript pager" over a
            // real PTY and observing `\x1b[5~` move its scroll position
            // from 100% to 0%. That claim could not be reproduced: driving
            // Codex 0.147.0 over a real PTY (TERM=xterm-256color, matching
            // how canopy spawns children — see `apply_canopy_session_env`
            // callers in `agent/mod.rs`) through startup, the update-skip
            // prompt, MCP server loading, a large composer paste, and an
            // async rate-limit/model-switch picker never produced a single
            // `\x1b[?1049h` (enter alternate screen), and neither `\x1b[5~`
            // nor a hand-built `\x1b[5;1u` produced any output distinguishable
            // from Codex's own idle spinner repaint. No "Transcript pager"
            // was reachable (Ctrl+T was a no-op). Since `forward_scroll` is
            // only ever invoked from `in_alternate_screen()` callers, and
            // Codex was never observed to enter that mode, this fallback
            // path does not appear to be exercised by Codex at all in the
            // states reachable without quota (restored 2026-09-12) — see
            // the C20 writeup.
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

/// Scan raw PTY output for a Kitty keyboard protocol "push flags" sequence
/// (`CSI > flags u`, e.g. `\x1b[>7u`) and return the flags value from the
/// last one found, if any. vt100 0.16.2 has no support of its own for this
/// protocol (no `u`-terminated CSI dispatch in its `perform.rs`), so this
/// scan runs independently over the same bytes handed to `vt100::Parser`,
/// in the reader thread in `agent/mod.rs`.
///
/// Only recognises a push sequence that lands whole inside `data`. In
/// practice a child emits this once, in its very first burst of output —
/// the 91-byte ground-truth capture this module's tests are built from is
/// exactly that burst arriving in a single PTY `read()` — so a parser that
/// tolerates the sequence splitting across reads was not worth the added
/// complexity.
pub(crate) fn parse_kitty_keyboard_push(data: &[u8]) -> Option<u8> {
    let mut found = None;
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'[' && data[i + 2] == b'>' {
            let start = i + 3;
            let mut j = start;
            while j < data.len() && data[j].is_ascii_digit() {
                j += 1;
            }
            if j > start && j < data.len() && data[j] == b'u' {
                if let Ok(flags) = std::str::from_utf8(&data[start..j])
                    .unwrap_or_default()
                    .parse::<u32>()
                {
                    found = Some(flags.min(u8::MAX as u32) as u8);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    found
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

    // C6b: `should_forward_scroll_to_child` used to also forward (and let
    // `forward_mouse` consume) a scroll tick under `MouseProtocolMode::None`
    // whenever the child owned the alternate screen, on the theory that the
    // PgUp/PgDn fallback was better than nothing. In practice `forward_mouse`
    // had no way to confirm the child understood that fallback, so a child
    // that didn't (or one where the guess was simply wrong) silently ate the
    // gesture — canopy's own fallback scroll in `scroll_terminal_like_agent`
    // never got a turn, because the event was already marked consumed. That
    // shipped and made scrolling over a real Codex session a dead gesture.
    // The predicate is now protocol-only; the alternate-screen fallback is
    // handled exclusively by `forward_scroll`/`scroll_terminal_like_agent`,
    // which never consumes anything since it isn't gated by a return value.

    #[test]
    fn no_protocol_declines_regardless_of_screen_mode() {
        // forward_mouse must never consume a scroll tick on a guess — see
        // the module doc above. This is the regression test for C6b.
        assert!(!should_forward_scroll_to_child(MPM::None));
    }

    #[test]
    fn active_protocol_forwards() {
        assert!(should_forward_scroll_to_child(MPM::Press));
        assert!(should_forward_scroll_to_child(MPM::PressRelease));
        assert!(should_forward_scroll_to_child(MPM::AnyMotion));
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

// C6b: end-to-end coverage of `InteractiveAgent::forward_mouse`'s scroll
// branch against a real spawned child, so the regression is pinned at the
// method boundary the router (`try_forward_mouse_to_pty` in event/mod.rs)
// actually calls — not just at the pure predicate.
#[cfg(test)]
mod forward_mouse_scroll_tests {
    use crate::domain::models::Cli;
    use crate::tui::agent::InteractiveAgent;
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    use ratatui::style::Color;

    fn spawn_cat_agent() -> InteractiveAgent {
        InteractiveAgent::spawn(
            Cli::new("cat"),
            ".",
            80,
            24,
            None,
            None,
            Color::Reset,
            Some("forward-mouse-scroll-test-agent"),
            &[],
            None,
            None,
            None,
        )
        .expect("spawn cat as a stand-in interactive child")
    }

    #[test]
    fn no_protocol_in_alternate_screen_does_not_consume() {
        // This is the regression test for the whole C6b spec: a full-screen
        // child with no mouse protocol (Codex, in practice) must not have
        // its scroll ticks swallowed here on the PgUp/PgDn guess — the
        // caller (`scroll_terminal_like_agent`) is the one that forwards
        // that fallback, and only it may claim credit for handling it.
        let mut agent = spawn_cat_agent();
        agent.vt.lock().expect("vt lock").process(b"\x1b[?1049h");
        assert!(agent.in_alternate_screen());

        let consumed = agent
            .forward_mouse(MouseEventKind::ScrollUp, MouseButton::Left, 5, 5)
            .expect("forward_mouse should not error");

        assert!(!consumed);
        agent.kill();
    }

    #[test]
    fn no_protocol_outside_alternate_screen_does_not_consume() {
        let mut agent = spawn_cat_agent();
        assert!(!agent.in_alternate_screen());

        let consumed = agent
            .forward_mouse(MouseEventKind::ScrollDown, MouseButton::Left, 5, 5)
            .expect("forward_mouse should not error");

        assert!(!consumed);
        agent.kill();
    }

    #[test]
    fn active_protocol_consumes_scroll_ticks() {
        let mut agent = spawn_cat_agent();
        // DECSET 1000 (press/release mouse mode) + 1006 (SGR encoding).
        agent
            .vt
            .lock()
            .expect("vt lock")
            .process(b"\x1b[?1000h\x1b[?1006h");

        let consumed = agent
            .forward_mouse(MouseEventKind::ScrollUp, MouseButton::Left, 5, 5)
            .expect("forward_mouse should not error");

        assert!(consumed);
        agent.kill();
    }
}

// C20: pure-function coverage for `parse_kitty_keyboard_push`, the raw-byte
// scan that stands in for vt100's lack of Kitty keyboard protocol support.
#[cfg(test)]
mod kitty_keyboard_push_tests {
    use super::parse_kitty_keyboard_push;

    // Ground truth: Codex 0.147.0's real negotiation burst, captured over a
    // real PTY (see the C20 writeup) — 91 bytes, arriving in one PTY read.
    const CODEX_NEGOTIATION: &[u8] = b"\x1b[?2004h\x1b[>4;0m\x1b[>7u\x1b[?1004h\x1b[6n\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?u\x1b[c\x1b[?2026h\x1b[39m\x1b[49m\x1b[0m\x1b[?25l\x1b[?2026l";

    #[test]
    fn recognises_the_codex_negotiation_burst() {
        assert_eq!(parse_kitty_keyboard_push(CODEX_NEGOTIATION), Some(7));
    }

    #[test]
    fn absence_leaves_it_unset() {
        // The same burst minus the `\x1b[>7u` push — everything else here
        // (modifyOtherKeys, focus reporting, cursor position report) must
        // not be mistaken for a keyboard-protocol push.
        assert_eq!(
            parse_kitty_keyboard_push(b"\x1b[?2004h\x1b[>4;0m\x1b[?1004h\x1b[6n"),
            None
        );
    }

    #[test]
    fn a_later_push_overrides_an_earlier_one() {
        assert_eq!(parse_kitty_keyboard_push(b"\x1b[>1u\x1b[>31u"), Some(31));
    }

    #[test]
    fn a_push_with_no_flags_digits_is_not_mistaken_for_one() {
        assert_eq!(parse_kitty_keyboard_push(b"\x1b[>u"), None);
    }
}

// C20: end-to-end coverage of `InteractiveAgent::kitty_keyboard_negotiated`
// against a real spawned child, mirroring the `forward_mouse_scroll_tests`
// pattern above — the state must come from genuine PTY output, not a mock.
#[cfg(test)]
mod kitty_keyboard_negotiation_tests {
    use crate::domain::models::Cli;
    use crate::tui::agent::InteractiveAgent;
    use ratatui::style::Color;
    use std::time::{Duration, Instant};

    fn spawn_cat_agent() -> InteractiveAgent {
        InteractiveAgent::spawn(
            Cli::new("cat"),
            ".",
            80,
            24,
            None,
            None,
            Color::Reset,
            Some("kitty-keyboard-test-agent"),
            &[],
            None,
            None,
            None,
        )
        .expect("spawn cat as a stand-in interactive child")
    }

    fn wait_for(mut check: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if check() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn negotiated_state_is_set_once_the_child_pushes_kitty_flags() {
        let mut agent = spawn_cat_agent();
        assert!(!agent.kitty_keyboard_negotiated());

        // `cat` echoes stdin straight back out to its own stdout, so
        // writing the push sequence to its stdin round-trips it through the
        // real reader thread in `agent/mod.rs` exactly as a genuine child's
        // own output would. The PTY is in canonical mode, so a trailing
        // newline is needed to flush the line through to `cat` — it ends up
        // in the echoed bytes too, but the scanner only cares about the
        // `CSI > 7 u` part.
        agent
            .write_to_pty(b"\x1b[>7u\n")
            .expect("write to cat's stdin");

        assert!(wait_for(
            || agent.kitty_keyboard_negotiated(),
            Duration::from_secs(2)
        ));
        agent.kill();
    }

    #[test]
    fn negotiated_state_stays_unset_without_a_push() {
        let mut agent = spawn_cat_agent();
        agent
            .write_to_pty(b"hello, no kitty push here\n")
            .expect("write to cat's stdin");
        // Give the reader thread a beat to process ordinary output too, so
        // this isn't just "we didn't wait long enough".
        std::thread::sleep(Duration::from_millis(300));
        assert!(!agent.kitty_keyboard_negotiated());
        agent.kill();
    }
}
