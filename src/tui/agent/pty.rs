/// Set when `SIGHUP` is received. Checked once per event-loop iteration so
/// closing the terminal window shuts canopy down through the same
/// `app.cleanup()` path as a normal quit, instead of the process either
/// ignoring the signal (never shutting down) or dying at `SIG_DFL` (skipping
/// cleanup and losing the sessions).
#[cfg(unix)]
pub(crate) static SIGHUP_RECEIVED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_sighup(_signum: libc::c_int) {
    SIGHUP_RECEIVED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Install a real `SIGHUP` handler (never `SIG_IGN` — that disposition is
/// inherited across `execve` and would leave every CLI canopy spawns deaf to
/// `SIGHUP` too) and ignore `SIGPIPE`. Idempotent: safe to call from every
/// interactive-session spawn site.
#[cfg(unix)]
pub(crate) fn install_signal_handlers() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| unsafe {
        libc::signal(
            libc::SIGHUP,
            handle_sighup as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    });
}
#[cfg(unix)]
pub(crate) fn send_sighup_to_group(child: &mut dyn portable_pty::Child) {
    let Some(pid) = child.process_id().map(|pid| pid as i32) else {
        return;
    };
    let _ = crate::daemon::process::send_signal_to_group(pid, libc::SIGHUP);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sighup_disposition() -> libc::sighandler_t {
        let mut oldact: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut oldact) };
        assert_eq!(rc, 0, "sigaction query failed");
        oldact.sa_sigaction
    }

    #[test]
    fn install_signal_handlers_installs_a_real_hup_handler() {
        install_signal_handlers();

        // Neither SIG_IGN nor SIG_DFL: SIG_IGN is precisely the disposition
        // that leaked into every spawned CLI across `exec` (it, unlike a
        // handler, survives execve), and a handler resets to SIG_DFL on
        // exec — which is what restores default behaviour for the spawned
        // CLIs. A real handler must be neither.
        let disposition = sighup_disposition();
        assert_ne!(disposition, libc::SIG_IGN);
        assert_ne!(disposition, libc::SIG_DFL);
    }

    #[test]
    fn install_signal_handlers_is_idempotent() {
        install_signal_handlers();
        install_signal_handlers();
        install_signal_handlers();

        let disposition = sighup_disposition();
        assert_ne!(disposition, libc::SIG_IGN);
        assert_ne!(disposition, libc::SIG_DFL);
    }
}

/// Convert a crossterm key event to raw bytes for the PTY.
pub fn key_to_bytes(
    code: ratatui::crossterm::event::KeyCode,
    modifiers: ratatui::crossterm::event::KeyModifiers,
) -> Vec<u8> {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    match code {
        KeyCode::Char(c) => {
            if modifiers.contains(KeyModifiers::CONTROL) {
                let ctrl = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
                vec![ctrl]
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                s.as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => vec![],
    }
}
