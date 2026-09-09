//! Interactive agent management — PTY + vt100 virtual terminal.
//!
//! Each agent runs in a PTY. A background thread reads PTY output and
//! feeds it into a `vt100::Parser` which maintains a virtual screen buffer.
//! The UI reads this screen buffer and renders it as ratatui cells inside
//! the right panel — fully embedded, with colors and cursor.

use anyhow::Result;
use chrono::{DateTime, Utc};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::style::Color;

use crate::domain::models::Cli;
use crate::shared::sync_identity::{
    CANOPY_AGENT_ID_ENV, CANOPY_SEED_ID_ENV, CANOPY_SESSION_NAME_ENV, CANOPY_WORKDIR_ENV,
};

#[cfg(unix)]
use crate::tui::agent::pty::{install_signal_handlers, send_sighup_to_group};

pub mod input;
pub mod naming;
pub mod pty;
pub mod sanitize;
pub mod screen;

/// Status of an interactive agent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Exited(i32),
}

/// A recorded user prompt with its response range in scrollback.
#[derive(Clone)]
#[allow(dead_code)]
pub struct PromptEntry {
    pub input: String,
    /// (start_line, end_line) in the vt100 scrollback buffer
    /// representing the agent's response to this prompt.
    pub output_range: (usize, usize),
    pub timestamp: DateTime<Utc>,
}

/// Maximum number of prompt entries to keep in the ring buffer.
const MAX_PROMPT_HISTORY: usize = 20;
const VT_SCROLLBACK_LINES: usize = 5_000;

/// How recently a session must have produced PTY output to count as
/// "working" (blinking green) rather than merely "healthy but idle" (solid
/// green) in the status color. Blue is reserved for background agents; see
/// `session_status_color` in `ui/sidebar.rs`. Kept short enough that the
/// color reacts within one interaction, long enough to survive brief pauses
/// between output chunks so a steadily streaming session doesn't flicker.
pub(crate) const ACTIVITY_IDLE_THRESHOLD_MS: i64 = 12_000;

/// vt100 callbacks that mirror a PTY program's OSC 52 clipboard writes to the
/// host system clipboard. Without this the parser silently drops OSC 52, so a
/// harness (Claude Code, opencode, …) reports "copied" but nothing reaches the
/// real clipboard.
#[derive(Default)]
pub(crate) struct ClipboardForwarder;

impl vt100::Callbacks for ClipboardForwarder {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _ty: &[u8], data: &[u8]) {
        let Some(text) = decode_osc52_payload(data) else {
            return;
        };
        // Set off-thread so clipboard I/O never blocks the held vt parser lock.
        std::thread::spawn(move || crate::tui::clipboard::set_text(&text));
    }
}

/// Decode an OSC 52 base64 payload into UTF-8 text, tolerating missing padding.
fn decode_osc52_payload(data: &[u8]) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data))
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// vt100 parser wired with our clipboard-forwarding callbacks.
pub(crate) type Vt = vt100::Parser<ClipboardForwarder>;

/// Before shrinking a vt100 screen from `old_rows` to `new_rows`, move the rows
/// that would fall off the bottom into the scrollback.
///
/// vt100's `set_size` shrink path ends in `Vec::resize`, which truncates the
/// tail rows — the newest output — and never hands them to the scrollback. A
/// Scroll Up first routes them through `scroll_up`, which does push evicted rows
/// onto the scrollback deque; the matching Cursor Up keeps the shell's prompt on
/// its logical line so it does not appear to jump. A grow (or an unchanged
/// height, e.g. a cols-only resize) is a no-op.
fn preserve_rows_on_shrink<C: vt100::Callbacks>(
    parser: &mut vt100::Parser<C>,
    old_rows: u16,
    new_rows: u16,
) {
    if new_rows >= old_rows {
        return;
    }
    let delta = old_rows - new_rows;
    parser.process(format!("\x1b[{delta}S\x1b[{delta}A").as_bytes());
}

fn apply_canopy_session_env(
    cmd: &mut CommandBuilder,
    agent_id: &str,
    session_name: &str,
    working_dir: &str,
    seed_id: Option<&str>,
) {
    cmd.env(CANOPY_AGENT_ID_ENV, agent_id);
    cmd.env(CANOPY_SESSION_NAME_ENV, session_name);
    cmd.env(CANOPY_WORKDIR_ENV, working_dir);
    if let Some(sid) = seed_id {
        cmd.env(CANOPY_SEED_ID_ENV, sid);
    }
}

/// An interactive agent with a virtual terminal screen.
pub struct InteractiveAgent {
    /// UUID-based permanent identifier
    pub id: String,
    /// Display name for personality (from RANDOM_NAMES)
    pub name: String,
    /// Seed identity ID (if bound to a seed)
    #[allow(dead_code)]
    pub seed_id: Option<String>,
    /// Seed display name (resolved from seed_id, shown in TUI instead of random name)
    pub seed_name: Option<String>,
    pub cli: Cli,
    #[allow(dead_code)]
    pub working_dir: String,
    #[allow(dead_code)]
    pub started_at: DateTime<Utc>,
    pub status: AgentStatus,
    /// Accent color for this agent's TUI elements (from `CliConfig`).
    pub accent_color: Color,
    /// Whether this is a raw terminal session (no AI CLI).
    #[allow(dead_code)]
    pub is_terminal: bool,
    /// Shell binary for terminal sessions (e.g. "zsh", "bash").
    pub shell: String,
    /// PTY writer — send bytes to the agent's stdin.
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Virtual terminal screen — fed by PTY output (for live rendering with colors).
    pub(crate) vt: Arc<Mutex<Vt>>,
    /// Child process handle.
    pub(crate) child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    /// PTY master — needed for resize.
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    /// Scroll offset (0 = bottom/live, positive = scrolled up).
    pub scroll_offset: usize,
    /// Last known PTY dimensions (for resize detection).
    pub last_pty_cols: u16,
    pub last_pty_rows: u16,
    /// Ring buffer of recent user prompts and their responses.
    pub prompt_history: Arc<Mutex<VecDeque<PromptEntry>>>,
    /// Current accumulated input (characters since last Enter).
    pub input_buffer: Arc<Mutex<String>>,
    /// Tracks when the PTY last received output (for detecting idle/waiting state).
    pub(crate) last_output_at: Arc<Mutex<DateTime<Utc>>>,
    /// Output arriving before this instant does not count as activity (B21):
    /// a PTY resize (entering a section, layout change) makes every TUI app
    /// repaint, and that repaint burst must not light up the activity pulse.
    pub(crate) activity_suppressed_until: Arc<Mutex<DateTime<Utc>>>,
    /// Tracks when the user last viewed/focused this agent.
    last_viewed_at: Arc<Mutex<DateTime<Utc>>>,
    /// Whether the exit notification has already been sent (avoids repeats).
    pub exit_notified: bool,
    /// Warp-like input mode: accumulate keystrokes in input_buffer, send on Enter.
    /// Only used for terminal sessions (is_terminal == true).
    pub warp_mode: bool,
    /// Cursor position within the warp input buffer (byte offset).
    pub warp_cursor: usize,
    /// Index into session history for Up/Down browsing (None = not browsing).
    pub history_index: Option<usize>,
    /// True once the current shell line has been materialized in the PTY
    /// and warp input should stay synchronized from PTY edits.
    pub warp_passthrough: bool,
    /// Flags from the most recent Kitty keyboard protocol "push" the child
    /// sent (`CSI > flags u`), if any — `None` until one is observed.
    /// Populated by a raw scan of PTY output in the reader thread (vt100
    /// 0.16 has no Kitty-protocol support of its own — see
    /// `input::parse_kitty_keyboard_push`), independent of the vt100
    /// parser's own state. Exposed via
    /// [`crate::tui::agent::InteractiveAgent::kitty_keyboard_negotiated`],
    /// consulted by `event::agent_focus::focused_child_claimed_keyboard` to
    /// decide whether a focus shortcut should yield to the child; not
    /// consulted by any scroll encoder (see the `MPM::None` arm of
    /// `encode_scroll_sequence` in `input.rs` for why Page Up/Page Down
    /// specifically have no distinct Kitty encoding to switch to).
    pub(crate) kitty_keyboard_flags: Arc<Mutex<Option<u8>>>,
}

impl InteractiveAgent {
    /// Spawn a new interactive agent in a PTY with a virtual terminal.
    ///
    /// `cols` and `rows` should match the panel area where the agent will render.
    /// `interactive_args` come from the registry (e.g. `--tui`, `-c`, etc.).
    /// `fallback_args` are tried if the primary args fail (e.g. kiro `chat`).
    /// `name` is an optional user-provided session name (random if None).
    /// `existing_ids` is used to avoid name collisions.
    /// `model` and `model_flag` allow passing a model selection (e.g. `-m gpt-4`).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        cli: Cli,
        working_dir: &str,
        cols: u16,
        rows: u16,
        interactive_args: Option<&str>,
        fallback_args: Option<&str>,
        accent_color: Color,
        name: Option<&str>,
        existing_ids: &[&str],
        model: Option<&str>,
        model_flag: Option<&str>,
        seed_id: Option<&str>,
    ) -> Result<Self> {
        #[cfg(unix)]
        install_signal_handlers();

        let id = uuid::Uuid::new_v4().to_string();
        let name = name
            .map(str::to_owned)
            .unwrap_or_else(|| naming::pick_random_name(existing_ids));
        let (seed_id_owned, seed_name) = match seed_id {
            Some(sid) => {
                let resolved = crate::domain::seeds::load_seed(sid)
                    .ok()
                    .map(|identity| identity.name);
                (Some(sid.to_string()), resolved)
            }
            None => (None, None),
        };

        let pty_system = native_pty_system();

        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(cli.command_name());
        // Apply registry-driven interactive args (e.g. "--tui", "-c", etc.)
        // If primary args fail and fallback is available, try that instead.
        let args_to_use = interactive_args.or(fallback_args);
        if let Some(args) = args_to_use {
            for arg in args.split_whitespace().filter(|a| !a.is_empty()) {
                cmd.arg(arg);
            }
        }
        if let (Some(flag), Some(m)) = (model_flag, model) {
            if !m.is_empty() {
                cmd.arg(flag);
                cmd.arg(m);
            }
        }
        cmd.cwd(working_dir);
        apply_canopy_session_env(&mut cmd, &id, &name, working_dir, seed_id);

        // Advertise truecolor capability so child CLIs (Kiro, etc.) use
        // 24-bit RGB color sequences for their accent colors instead of
        // limited 16-color ANSI codes that get mapped to wrong hues.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        // Pass Canopy's UX accent color to child CLIs so they respect
        // the original color scheme instead of using their own ANSI color map.
        // Extract RGB components from ratatui::style::Color
        if let Color::Rgb(r, g, b) = accent_color {
            cmd.env("CANOPY_ACCENT_R", r.to_string());
            cmd.env("CANOPY_ACCENT_G", g.to_string());
            cmd.env("CANOPY_ACCENT_B", b.to_string());
        }

        let child = pair.slave.spawn_command(cmd)?;
        // Drop slave so the PTY closes when the child exits
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let master = pair.master;

        let vt = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            VT_SCROLLBACK_LINES,
            ClipboardForwarder,
        )));
        let vt_clone = Arc::clone(&vt);

        let last_output_at = Arc::new(Mutex::new(Utc::now()));
        let last_output_at_clone = Arc::clone(&last_output_at);
        let activity_suppressed_until = Arc::new(Mutex::new(Utc::now()));
        let suppressed_until_clone = Arc::clone(&activity_suppressed_until);
        let kitty_keyboard_flags = Arc::new(Mutex::new(None));
        let kitty_keyboard_flags_clone = Arc::clone(&kitty_keyboard_flags);

        // Background thread: read PTY output → feed into vt100 parser
        std::thread::spawn(move || {
            let mut tmp = [0u8; 4096];
            loop {
                match reader.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut parser) = vt_clone.lock() {
                            parser.process(&tmp[..n]);
                        }
                        if let Some(flags) = input::parse_kitty_keyboard_push(&tmp[..n]) {
                            if let Ok(mut f) = kitty_keyboard_flags_clone.lock() {
                                *f = Some(flags);
                            }
                        }
                        // Stamp last output time so is_waiting_for_input()
                        // can detect idle — unless this output falls inside a
                        // post-resize suppression window (a repaint, not real
                        // activity — B21).
                        let suppressed = suppressed_until_clone
                            .lock()
                            .map(|until| Utc::now() < *until)
                            .unwrap_or(false);
                        if !suppressed {
                            if let Ok(mut t) = last_output_at_clone.lock() {
                                *t = Utc::now();
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            id,
            name,
            seed_id: seed_id_owned,
            seed_name,
            cli,
            working_dir: working_dir.to_string(),
            started_at: Utc::now(),
            status: AgentStatus::Running,
            accent_color,
            is_terminal: false,
            shell: String::new(),
            writer: Arc::new(Mutex::new(writer)),
            vt,
            child: Arc::new(Mutex::new(child)),
            master: Arc::new(Mutex::new(master)),
            scroll_offset: 0,
            last_pty_cols: cols,
            last_pty_rows: rows,
            prompt_history: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_PROMPT_HISTORY))),
            input_buffer: Arc::new(Mutex::new(String::new())),
            last_output_at,
            activity_suppressed_until,
            last_viewed_at: Arc::new(Mutex::new(Utc::now())),
            exit_notified: false,
            warp_mode: false,
            warp_cursor: 0,
            history_index: None,
            warp_passthrough: false,
            kitty_keyboard_flags,
        })
    }

    /// Spawn a raw terminal session (no AI CLI model).
    ///
    /// Uses `shell` as the command (e.g. `"bash"`, `"zsh"`).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_terminal(
        shell: &str,
        working_dir: &str,
        cols: u16,
        rows: u16,
        name: Option<&str>,
        existing_ids: &[&str],
        accent_color: Color,
    ) -> Result<Self> {
        #[cfg(unix)]
        install_signal_handlers();

        let id = uuid::Uuid::new_v4().to_string();
        let session_name = name
            .map(str::to_owned)
            .unwrap_or_else(|| naming::pick_terminal_name(existing_ids));
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(working_dir);
        apply_canopy_session_env(&mut cmd, &id, &session_name, working_dir, None);
        // Compact prompt since warp mode shows its own prompt line
        cmd.env("PS1", "$ ");
        cmd.env("PROMPT_COMMAND", "");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let master = pair.master;

        let vt = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            VT_SCROLLBACK_LINES,
            ClipboardForwarder,
        )));
        let vt_clone = Arc::clone(&vt);

        let last_output_at = Arc::new(Mutex::new(Utc::now()));
        let last_output_at_clone = Arc::clone(&last_output_at);
        let activity_suppressed_until = Arc::new(Mutex::new(Utc::now()));
        let suppressed_until_clone = Arc::clone(&activity_suppressed_until);
        let kitty_keyboard_flags = Arc::new(Mutex::new(None));
        let kitty_keyboard_flags_clone = Arc::clone(&kitty_keyboard_flags);

        std::thread::spawn(move || {
            let mut tmp = [0u8; 4096];
            loop {
                match reader.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut parser) = vt_clone.lock() {
                            parser.process(&tmp[..n]);
                        }
                        if let Some(flags) = input::parse_kitty_keyboard_push(&tmp[..n]) {
                            if let Ok(mut f) = kitty_keyboard_flags_clone.lock() {
                                *f = Some(flags);
                            }
                        }
                        // See the sibling reader thread above: repaint bursts
                        // inside a post-resize window are not activity (B21).
                        let suppressed = suppressed_until_clone
                            .lock()
                            .map(|until| Utc::now() < *until)
                            .unwrap_or(false);
                        if !suppressed {
                            if let Ok(mut t) = last_output_at_clone.lock() {
                                *t = Utc::now();
                            }
                        }
                    }
                }
            }
        });

        let cli = Cli::new(shell);

        Ok(Self {
            id,
            name: session_name,
            seed_id: None,
            seed_name: None,
            cli,
            working_dir: working_dir.to_string(),
            started_at: Utc::now(),
            status: AgentStatus::Running,
            accent_color,
            is_terminal: true,
            shell: shell.to_string(),
            writer: Arc::new(Mutex::new(writer)),
            vt,
            child: Arc::new(Mutex::new(child)),
            master: Arc::new(Mutex::new(master)),
            scroll_offset: 0,
            last_pty_cols: cols,
            last_pty_rows: rows,
            prompt_history: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_PROMPT_HISTORY))),
            input_buffer: Arc::new(Mutex::new(String::new())),
            last_output_at,
            activity_suppressed_until,
            last_viewed_at: Arc::new(Mutex::new(Utc::now())),
            exit_notified: false,
            warp_mode: true,
            warp_cursor: 0,
            history_index: None,
            warp_passthrough: false,
            kitty_keyboard_flags,
        })
    }

    /// Mark the agent as having been viewed/attended by the user.
    /// This suppresses the waiting indicator until new output arrives.
    pub fn mark_viewed(&self) {
        if let Ok(mut t) = self.last_viewed_at.lock() {
            *t = Utc::now();
        }
    }

    /// The OS process id of the underlying CLI child, if the PTY exposes it.
    pub fn pid(&self) -> Option<i64> {
        self.child
            .lock()
            .ok()
            .and_then(|c| c.process_id())
            .map(|p| p as i64)
    }

    /// Send raw bytes to the agent's PTY stdin.
    pub fn write_to_pty(&self, data: &[u8]) -> Result<()> {
        if let Ok(mut w) = self.writer.lock() {
            w.write_all(data)?;
            w.flush()?;
        }
        Ok(())
    }

    /// Check if the process has exited.
    pub fn poll(&mut self) {
        if self.status != AgentStatus::Running {
            return;
        }
        if let Ok(mut child) = self.child.lock() {
            if let Ok(Some(status)) = child.try_wait() {
                self.status = AgentStatus::Exited(status.exit_code().try_into().unwrap_or(-1));
            }
        }
    }
    /// on `child.wait()`. The background PTY reader thread will detect EOF and
    /// exit on its own; the OS reaps the child process.
    pub fn kill(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            #[cfg(unix)]
            send_sighup_to_group(child.as_mut());
            let _ = child.kill();
        }
        self.status = AgentStatus::Exited(-9);
    }

    /// Ask the agent to persist a final summary, then terminate it after a short delay.
    ///
    /// This enables "shadow summary" behavior: the UI can close immediately while
    /// the process briefly remains alive to run finalization instructions.
    pub fn schedule_shadow_shutdown(&self, final_instruction: &str, linger: Duration) {
        let _ = self.write_to_pty(final_instruction.as_bytes());
        let _ = self.write_to_pty(b"\n");

        let child = Arc::clone(&self.child);
        std::thread::spawn(move || {
            std::thread::sleep(linger);
            if let Ok(mut child) = child.lock() {
                #[cfg(unix)]
                send_sighup_to_group(child.as_mut());
                let _ = child.kill();
            }
        });
    }

    /// Resize the PTY and virtual terminal (e.g. on terminal window resize).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let old_rows = self.last_pty_rows;
        self.last_pty_cols = cols;
        self.last_pty_rows = rows;
        // A resize makes the app repaint; that output burst is not activity.
        // Suppress activity stamping briefly so switching sections doesn't
        // light up every session's pulse (B21).
        if let Ok(mut until) = self.activity_suppressed_until.lock() {
            *until = Utc::now() + chrono::Duration::seconds(1);
        }
        // Resize the actual PTY so the process knows about the new size
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        // Resize the virtual terminal screen. On a shrink, first scroll the rows
        // that would otherwise be truncated into the scrollback — both steps run
        // under one `vt` acquisition so a PTY reader thread can't slip output in
        // between the scroll and the resize.
        if let Ok(mut vt) = self.vt.lock() {
            preserve_rows_on_shrink(&mut vt, old_rows, rows);
            vt.screen_mut().set_size(rows, cols);
        }
    }

    /// Update the working directory. Used when CD command is executed.
    pub fn update_working_dir(&mut self, new_dir: &str) {
        self.working_dir = new_dir.to_string();
    }
}

pub use pty::key_to_bytes;
pub use screen::ScreenSnapshot;

#[cfg(test)]
mod tests {
    use super::{decode_osc52_payload, preserve_rows_on_shrink, ClipboardForwarder};
    use base64::Engine as _;

    /// A parser wired the way `InteractiveAgent` builds its `vt` (see the
    /// `new_with_callbacks` call in `InteractiveAgent::new`).
    fn agent_parser(rows: u16) -> vt100::Parser<ClipboardForwarder> {
        vt100::Parser::new_with_callbacks(rows, 80, super::VT_SCROLLBACK_LINES, ClipboardForwarder)
    }

    /// Number of rows currently held in the parser's scrollback deque.
    /// `Screen::scrollback()` reports the *scroll position*, so read it back at
    /// its clamped maximum, then restore the live view.
    fn scrollback_len(parser: &mut vt100::Parser<ClipboardForwarder>) -> usize {
        parser.screen_mut().set_scrollback(usize::MAX);
        let len = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(0);
        len
    }

    #[test]
    fn decodes_padded_osc52_payload() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("hello world");
        assert_eq!(
            decode_osc52_payload(encoded.as_bytes()).as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn decodes_unpadded_osc52_payload() {
        // Some emitters strip the `=` padding; we must still decode it.
        let unpadded = base64::engine::general_purpose::STANDARD_NO_PAD.encode("multi\nline");
        assert!(!unpadded.ends_with('='));
        assert_eq!(
            decode_osc52_payload(unpadded.as_bytes()).as_deref(),
            Some("multi\nline")
        );
    }

    #[test]
    fn rejects_non_base64_payload() {
        assert_eq!(decode_osc52_payload(b"!!! not base64 !!!"), None);
    }

    // T1: shrinking the virtual screen must move the rows that fall off into
    // scrollback instead of letting `set_size` truncate them.
    #[test]
    fn shrink_preserves_content_in_scrollback() {
        let mut parser = agent_parser(10);
        for i in 0..10 {
            parser.process(format!("line{i}\r\n").as_bytes());
        }
        // Writing the 10th line's newline already pushed `line0` back.
        assert_eq!(scrollback_len(&mut parser), 1);

        // The shrink routine, then the resize it guards.
        preserve_rows_on_shrink(&mut parser, 10, 7);
        parser.screen_mut().set_size(7, 80);

        // The 3 rows that left the visible screen joined `line0` in scrollback
        // rather than being lost: 1 + 3 = 4.
        assert_eq!(scrollback_len(&mut parser), 4);
        parser.screen_mut().set_scrollback(usize::MAX);
        let scrolled_back = parser.screen().contents();
        for line in ["line1", "line2", "line3"] {
            assert!(
                scrolled_back.contains(line),
                "{line} was dropped instead of scrolled back: {scrolled_back:?}"
            );
        }
        // The newest line was at the bottom of the screen and must survive.
        parser.screen_mut().set_scrollback(0);
        assert!(parser.screen().contents().contains("line9"));
    }

    // T2: the reported case — a shrink then a regrow (input box grows, then
    // collapses on submit). The last line written before the shrink must still
    // be on screen and the pushed-off rows must stay reachable by scrolling.
    #[test]
    fn shrink_then_regrow_preserves_last_line() {
        let mut parser = agent_parser(10);
        for i in 0..10 {
            parser.process(format!("line{i}\r\n").as_bytes());
        }

        preserve_rows_on_shrink(&mut parser, 10, 7);
        parser.screen_mut().set_size(7, 80);
        preserve_rows_on_shrink(&mut parser, 7, 10); // regrow: no-op
        parser.screen_mut().set_size(10, 80);

        let contents = parser.screen().contents();
        assert!(
            contents.contains("line9"),
            "last pre-shrink line was destroyed by the shrink: {contents:?}"
        );
        assert!(
            contents
                .lines()
                .next_back()
                .is_some_and(|l| !l.trim().is_empty()),
            "screen bottom collapsed to a band of blanks: {contents:?}"
        );
        // The rows the shrink pushed off are still in scrollback.
        parser.screen_mut().set_scrollback(usize::MAX);
        assert!(parser.screen().contents().contains("line1"));
    }

    // T3: a grow-only resize must not feed any scroll sequence to the parser.
    #[test]
    fn grow_only_resize_emits_no_scroll() {
        let mut parser = agent_parser(10);
        for i in 0..10 {
            parser.process(format!("line{i}\r\n").as_bytes());
        }
        let before = scrollback_len(&mut parser);

        preserve_rows_on_shrink(&mut parser, 10, 15);
        parser.screen_mut().set_size(15, 80);

        assert_eq!(
            scrollback_len(&mut parser),
            before,
            "a grow moved rows into scrollback"
        );
    }
}
