use anyhow::Result;

/// Grace period between `SIGTERM` and `SIGKILL` when terminating a node
/// run's process group (B12): timeout, iteration-budget exhaustion,
/// `loop_pause`, `loop_reset`, run failure elsewhere, and daemon shutdown
/// all go through [`terminate_process_group_async`] with this grace.
pub(crate) const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

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

/// Send `signal` to the process group led by `pid` (i.e. `killpg`). A group
/// that's already gone (`ESRCH`) is treated as success — there's nothing
/// left to signal, which is exactly the caller's desired end state.
#[cfg(unix)]
pub(crate) fn send_signal_to_group(pid: i32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::killpg(pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

/// Best-effort termination (B12) of the process group led by `pid`: `SIGTERM`
/// now, `SIGKILL` after `grace` if the group is still alive. The grace wait
/// runs on a detached task so the caller (e.g. `loop_pause`, an iteration
/// budget check) never blocks on it — the killed process's own
/// `wait()`/`wait_with_output()` elsewhere unblocks as soon as it actually
/// dies, whether that's from the `SIGTERM` or the follow-up `SIGKILL`.
///
/// Unix-only: killing a whole process group by PID with no live `Child`
/// handle has no portable equivalent. On non-unix targets this is a no-op —
/// the one path that still gets best-effort termination on Windows is a
/// timeout with a live `Child` in hand, which kills the direct child via
/// `tokio::process::Child::start_kill`.
pub(crate) fn terminate_process_group_async(pid: i64, grace: std::time::Duration) {
    #[cfg(unix)]
    {
        let pid = pid as i32;
        let _ = send_signal_to_group(pid, libc::SIGTERM);
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let _ = send_signal_to_group(pid, libc::SIGKILL);
        });
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = grace;
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

    #[cfg(unix)]
    #[test]
    fn is_process_running_returns_true_for_current_process() {
        let current_pid = std::process::id();
        assert!(
            is_process_running(current_pid),
            "current process should be running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn is_process_running_returns_false_for_invalid_pid() {
        // Very large PID that's unlikely to exist
        assert!(
            !is_process_running(999999999),
            "nonexistent PID should not be reported as running"
        );
        // Negative PIDs are invalid (but the function takes u32, so we can't test negative)
        // Instead test a PID that's definitely not running
        assert!(
            !is_process_running(4294967294),
            "nonexistent high PID should not be reported as running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn parse_pids_from_ss_extracts_pids_from_ss_output() {
        let ss_output = r#"State   Recv-Q  Send-Q  Local Address:Port  Peer Address:Port  Process
LISTEN  0       128     0.0.0.0:8080        0.0.0.0:*            users:(("nginx",pid=1234,fd=6))
LISTEN  0       128     0.0.0.0:9090        0.0.0.0:*            users:(("node",pid=5678,fd=12))
"#;
        let pids = parse_pids_from_ss(ss_output);
        assert_eq!(pids, vec![1234, 5678]);
    }

    #[cfg(unix)]
    #[test]
    fn parse_pids_from_ss_handles_empty_output() {
        let pids = parse_pids_from_ss("");
        assert!(pids.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn parse_pids_from_ss_handles_no_pid_field() {
        let ss_output = r#"State   Recv-Q  Send-Q  Local Address:Port  Peer Address:Port
LISTEN  0       128     0.0.0.0:8080        0.0.0.0:*
"#;
        let pids = parse_pids_from_ss(ss_output);
        assert!(pids.is_empty());
    }

    #[test]
    fn write_pid_file_creates_file_with_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_pid_file(dir.path()).expect("write_pid_file should succeed");
        let pid_path = dir.path().join("daemon.pid");
        assert!(pid_path.exists(), "PID file should be created");
        let content = std::fs::read_to_string(&pid_path).expect("should read PID file");
        let pid: u32 = content.trim().parse().expect("PID should be valid u32");
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn read_pid_returns_none_when_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_pid(dir.path()).is_none());
    }

    #[test]
    fn read_pid_returns_pid_from_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("daemon.pid");
        std::fs::write(&pid_path, "12345\n").expect("should write PID file");
        assert_eq!(read_pid(dir.path()), Some(12345));
    }

    #[test]
    fn read_pid_returns_none_for_invalid_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("daemon.pid");
        std::fs::write(&pid_path, "not-a-number\n").expect("should write PID file");
        assert!(read_pid(dir.path()).is_none());
    }

    #[test]
    fn remove_pid_file_removes_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("daemon.pid");
        std::fs::write(&pid_path, "12345\n").expect("should write PID file");
        assert!(pid_path.exists());
        remove_pid_file(dir.path());
        assert!(!pid_path.exists(), "PID file should be removed");
    }

    #[test]
    fn remove_pid_file_succeeds_when_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        remove_pid_file(dir.path()); // Should not panic
    }

    #[test]
    fn print_last_n_lines_handles_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonexistent.log");
        let result = print_last_n_lines(&path, 10);
        assert!(result.is_err());
    }

    #[test]
    fn print_last_n_lines_reads_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.log");
        std::fs::write(&path, "line1\nline2\nline3\nline4\nline5\n").expect("should write log");
        let result = print_last_n_lines(&path, 3);
        assert!(result.is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn is_systemd_available_returns_bool() {
        // Just verify it doesn't panic and returns a bool
        let _ = is_systemd_available();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn is_service_enabled_returns_bool() {
        // Just verify it doesn't panic and returns a bool
        let _ = is_service_enabled();
    }
}
