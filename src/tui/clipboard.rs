//! System clipboard helper, shared by the TUI selection copy and the OSC 52
//! forwarder that mirrors a PTY program's clipboard writes to the host.

/// Write `text` to the system clipboard. Fire-and-forget: failures (no display
/// server, unsupported platform) are ignored, matching the rest of the TUI.
pub(crate) fn set_text(text: &str) {
    let _ = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.to_owned()));
}
