use anyhow::Result;

/// Advisory singleton lock held for the lifetime of a running daemon.
///
/// The lock is acquired via `flock(2)` on a dedicated `daemon.lock` file
/// (never on `background_agents.db`, which the TUI opens directly as a
/// co-equal writer). Holding the `File` open keeps the OS-level flock in
/// place; dropping this guard — including implicitly when the process exits
/// or crashes — closes the fd and the kernel releases the lock immediately.
/// This means a crashed daemon can never leave a stale lock behind.
#[derive(Debug)]
pub(crate) struct DaemonLock {
    #[allow(dead_code)]
    lock_file: std::fs::File,
}

/// Acquire the daemon singleton lock in `data_dir`, failing fast if another
/// `canopy serve` process already holds it.
pub(crate) fn acquire_daemon_lock(data_dir: &std::path::Path) -> Result<DaemonLock> {
    let path = data_dir.join("daemon.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;

        let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            // On Linux, EWOULDBLOCK and EAGAIN are the same errno value; both
            // are matched here for portability across platforms where they
            // may differ.
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                anyhow::bail!(
                    "another canopy daemon is already running (lock held on {})",
                    path.display()
                );
            }
            return Err(err.into());
        }
    }

    Ok(DaemonLock { lock_file })
}

pub(crate) fn is_process_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

pub(crate) fn kill_port_occupant(port: u16) {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("ss")
            .args(["-tlnp", &format!("sport = :{port}")])
            .output();

        if let Ok(out) = output {
            let text = String::from_utf8_lossy(&out.stdout);
            let self_pid = std::process::id();
            for pid in parse_pids_from_ss(&text) {
                if pid != self_pid && pid != 0 {
                    terminate_process(pid, port);
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = port;
    }
}

#[cfg(unix)]
fn parse_pids_from_ss(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|line| {
            let pid_start = line.find("pid=")?;
            let rest = &line[pid_start + 4..];
            let end = rest.find(|c: char| !c.is_ascii_digit())?;
            rest[..end].parse::<u32>().ok()
        })
        .collect()
}

#[cfg(unix)]
fn terminate_process(pid: u32, port: u16) {
    eprintln!("Port {port} occupied by PID {pid} — sending SIGTERM");
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    std::thread::sleep(std::time::Duration::from_millis(500));
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        eprintln!("PID {pid} did not exit — sending SIGKILL");
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub(crate) fn send_signal(pid: u32) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        eprintln!("Cannot send signal on this platform");
    }
}

pub(crate) fn write_pid_file(data_dir: &std::path::Path) -> Result<()> {
    let pid = std::process::id();
    std::fs::write(data_dir.join("daemon.pid"), pid.to_string())?;
    Ok(())
}

pub(crate) fn remove_pid_file(data_dir: &std::path::Path) {
    let _ = std::fs::remove_file(data_dir.join("daemon.pid"));
}

pub(crate) fn read_pid(data_dir: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(data_dir.join("daemon.pid"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[cfg(target_os = "linux")]
pub(crate) fn is_systemd_available() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub(crate) fn is_service_enabled() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "canopy.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) fn print_last_n_lines(path: &std::path::Path, n: usize) -> Result<()> {
    use std::io::{BufRead, BufReader};

    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().collect::<std::io::Result<Vec<_>>>()?;

    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_daemon_lock_succeeds_on_fresh_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = acquire_daemon_lock(dir.path());
        assert!(lock.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn second_acquire_fails_while_first_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _first = acquire_daemon_lock(dir.path()).expect("first acquire should succeed");

        let second = acquire_daemon_lock(dir.path());
        let err = second.expect_err("second acquire should fail while first is held");
        assert!(
            err.to_string().contains("already running"),
            "unexpected error message: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn acquire_succeeds_again_after_guard_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = acquire_daemon_lock(dir.path()).expect("first acquire should succeed");
        drop(first);

        let second = acquire_daemon_lock(dir.path());
        assert!(
            second.is_ok(),
            "reacquiring after drop should succeed: {:?}",
            second.err()
        );
    }
}
