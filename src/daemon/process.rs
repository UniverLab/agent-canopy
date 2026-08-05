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

/// Whether a canopy daemon may currently be reading or writing files under
/// `data_dir` — in particular, a pre-migration legacy path a data-layout
/// change moved.
///
/// Source trees here get rebuilt and re-run while the previously installed
/// binary's daemon (and TUI) are still live: "an old reader of the legacy
/// path may still exist" is the default assumption for any migration in
/// this project, not an edge case to special-case away. Callers doing a
/// one-time file-layout migration must check this before deleting a legacy
/// path, and defer the deletion (retrying on a later, quieter run) rather
/// than skip it forever.
pub(crate) fn other_instance_may_be_running(data_dir: &std::path::Path) -> bool {
    read_pid(data_dir).is_some_and(is_process_running)
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

// ── Port → PID resolution and service-manager fact-checking ──────────────
//
// `canopy daemon status` used to trust the PID file alone. That's a lie
// whenever the process actually bound to the port isn't the one the PID
// file (or a service manager) thinks is running — e.g. an orphaned `canopy
// serve` left over from a manual `daemon start` holds the port while the
// systemd unit that's supposed to own it fails to bind and retries forever.
// The functions below establish the ground truth (who's actually listening,
// who the service manager actually owns) without shelling out to
// `systemctl` — absent in some CI containers — so status/stop/doctor can
// compare PID-file, port, and service-manager facts and report disagreement
// instead of guessing.

/// Resolve the PID of whichever process currently holds `port` for
/// listening.
#[cfg(target_os = "linux")]
pub(crate) fn resolve_port_pid(port: u16) -> Option<u32> {
    let inode = find_listening_inode(port)?;
    find_pid_for_inode(inode)
}

#[cfg(target_os = "linux")]
fn find_listening_inode(port: u16) -> Option<u64> {
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Some(inode) = parse_listening_inode(&content, port) {
                return Some(inode);
            }
        }
    }
    None
}

/// Parse a `/proc/net/tcp`(6)-style table for the inode of a socket in
/// `LISTEN` state (`st` == `0A`) bound to `port`. Pure text parsing so it
/// can be exercised in tests without a real `/proc`.
#[cfg(target_os = "linux")]
fn parse_listening_inode(content: &str, port: u16) -> Option<u64> {
    const LISTEN_STATE: &str = "0A";
    let port_hex = format!("{port:04X}");
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 || fields[3] != LISTEN_STATE {
            continue;
        }
        let Some((_, local_port)) = fields[1].split_once(':') else {
            continue;
        };
        if local_port.eq_ignore_ascii_case(&port_hex) {
            if let Ok(inode) = fields[9].parse::<u64>() {
                return Some(inode);
            }
        }
    }
    None
}

/// Find the PID holding an open file descriptor on `socket:[inode]` by
/// scanning `/proc/<pid>/fd`. Processes owned by other users are silently
/// skipped (permission denied reading their fd dir) rather than erroring —
/// this daemon only ever runs as the invoking user anyway.
#[cfg(target_os = "linux")]
fn find_pid_for_inode(inode: u64) -> Option<u32> {
    let target = format!("socket:[{inode}]");
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path()) {
                if link.to_string_lossy() == target {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// Best-effort port occupant lookup for non-Linux Unix (macOS): reuses the
/// same `ss` parsing that already backs [`kill_port_occupant`]. Returns
/// `None` — never a guess — when `ss` isn't installed.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn resolve_port_pid(port: u16) -> Option<u32> {
    let output = std::process::Command::new("ss")
        .args(["-tlnp", &format!("sport = :{port}")])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_pids_from_ss(&text).into_iter().next()
}

#[cfg(not(unix))]
pub(crate) fn resolve_port_pid(_port: u16) -> Option<u32> {
    None
}

const SYSTEMD_UNIT_NAME: &str = "canopy.service";
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.canopy";

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config/systemd/user")
            .join(SYSTEMD_UNIT_NAME),
    )
}

#[cfg(target_os = "linux")]
fn systemd_service_installed() -> bool {
    systemd_unit_path().is_some_and(|p| p.exists())
}

/// Find the PID systemd currently manages for `canopy.service`, by reading
/// each process's own `/proc/<pid>/cgroup` — the same fact `systemctl
/// status` reports as `MainPID`, without shelling out to `systemctl` or
/// speaking D-Bus. Cgroup v1 and v2 both encode the unit name as the last
/// path segment of the process's cgroup, so this works regardless of the
/// slice layout a given distro uses.
#[cfg(target_os = "linux")]
fn systemd_managed_pid() -> Option<u32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Ok(cgroup) = std::fs::read_to_string(entry.path().join("cgroup")) {
            if is_unit_cgroup(&cgroup, SYSTEMD_UNIT_NAME) {
                return Some(pid);
            }
        }
    }
    None
}

/// Does a `/proc/<pid>/cgroup` file's content place the process in
/// `unit_name`'s cgroup? Each line is `hierarchy-id:controllers:path`; the
/// unit name is matched as the final `/`-separated segment of `path` so a
/// unit whose name happens to be a substring of another's doesn't match.
fn is_unit_cgroup(cgroup_content: &str, unit_name: &str) -> bool {
    cgroup_content.lines().any(|line| {
        line.rsplit(':')
            .next()
            .and_then(|path| path.rsplit('/').next())
            == Some(unit_name)
    })
}

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")),
    )
}

#[cfg(target_os = "macos")]
fn launchd_service_installed() -> bool {
    launchd_plist_path().is_some_and(|p| p.exists())
}

/// Best-effort `launchctl list <label>` PID lookup. macOS has no `/proc`
/// equivalent for this, so unlike the systemd path this does shell out —
/// `launchctl` is always present on macOS and this code never runs in the
/// Linux CI containers the "don't shell out" constraint is about. Returns
/// `None` (never a guess) if the command fails or the job isn't loaded.
#[cfg(target_os = "macos")]
fn launchd_managed_pid() -> Option<u32> {
    let output = std::process::Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_launchctl_pid(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the `"PID" = N;` line out of `launchctl list <label>`'s plist-ish
/// text output. Pure parsing, tested independently of `launchctl` itself
/// (and kept compiled on non-macOS targets only for that — `cfg(test)` — so
/// it isn't dead code on the platforms that actually ship the daemon).
#[cfg(any(target_os = "macos", test))]
fn parse_launchctl_pid(text: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("\"PID\" = ")?
            .trim_end_matches(';')
            .parse::<u32>()
            .ok()
    })
}

/// Ask the platform service manager to stop the canopy unit/agent — used
/// instead of signalling the managed PID directly when restoring the
/// daemon after `canopy clean`'s reclaim window (B-decision 4/5): the
/// installed unit has `Restart=on-failure`, so a bare `SIGTERM` reads to
/// systemd as an unclean exit and it respawns the process out from under
/// the exclusive `VACUUM` this stop was for. Going through the manager's
/// own stop verb is the only way to get a stop it won't immediately undo.
#[cfg(target_os = "linux")]
pub(crate) fn service_manager_stop() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "stop", SYSTEMD_UNIT_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub(crate) fn service_manager_start() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "start", SYSTEMD_UNIT_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub(crate) fn service_manager_stop() -> bool {
    let Some(path) = launchd_plist_path() else {
        return false;
    };
    std::process::Command::new("launchctl")
        .args(["unload", &path.display().to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub(crate) fn service_manager_start() -> bool {
    let Some(path) = launchd_plist_path() else {
        return false;
    };
    std::process::Command::new("launchctl")
        .args(["load", &path.display().to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn service_manager_stop() -> bool {
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn service_manager_start() -> bool {
    false
}

/// Is a service-manager unit/agent installed for canopy on this platform,
/// regardless of whether it currently owns a running process? Used by
/// `canopy clean`'s reclaim window to refuse stopping a service-manager-
/// owned daemon whose unit has since gone missing (B-decision 5: detect a
/// missing unit before stopping, not after).
#[cfg(target_os = "linux")]
pub(crate) fn service_unit_installed() -> bool {
    systemd_service_installed()
}

#[cfg(target_os = "macos")]
pub(crate) fn service_unit_installed() -> bool {
    launchd_service_installed()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn service_unit_installed() -> bool {
    false
}

/// Facts about the service manager (if any) that owns the canopy daemon on
/// this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServiceManagerFacts {
    pub(crate) name: &'static str,
    /// The PID the manager currently owns for this unit, or `None` if the
    /// unit is installed but has no live managed process (e.g. mid
    /// restart-backoff after a failed bind).
    pub(crate) pid: Option<u32>,
}

/// Gather service-manager facts, or `None` if no service manager is
/// installed for canopy at all (never running, or this platform has no
/// supported manager) — the "no service manager exists" case status/doctor
/// must degrade gracefully for rather than inventing a manager PID.
pub(crate) fn service_manager_facts() -> Option<ServiceManagerFacts> {
    #[cfg(target_os = "linux")]
    {
        if systemd_service_installed() {
            return Some(ServiceManagerFacts {
                name: "systemd",
                pid: systemd_managed_pid(),
            });
        }
    }
    #[cfg(target_os = "macos")]
    {
        if launchd_service_installed() {
            return Some(ServiceManagerFacts {
                name: "launchd",
                pid: launchd_managed_pid(),
            });
        }
    }
    None
}

/// The result of comparing the PID-file, port-occupant, and (if any)
/// service-manager facts about the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DaemonState {
    /// Nothing is listening on the port and no PID file names a live
    /// process.
    Stopped,
    /// Every fact that's available agrees on a single PID.
    Running { pid: u32 },
    /// The facts disagree — the one state `daemon status` must never
    /// silently paper over as `RUNNING`.
    Discrepancy(DaemonDiscrepancy),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonDiscrepancy {
    pub(crate) state_pid: Option<u32>,
    pub(crate) port_pid: Option<u32>,
    pub(crate) manager_name: Option<&'static str>,
    pub(crate) manager_pid: Option<u32>,
    /// The port is held by a live process a service manager exists for but
    /// does not own — a `canopy serve` that's outlived (or never belonged
    /// to) its unit, blocking the unit from ever binding.
    pub(crate) is_orphan: bool,
}

impl DaemonDiscrepancy {
    /// Human-readable lines naming every PID at play, for callers that want
    /// to print the full picture (`daemon status`, `doctor`).
    pub(crate) fn describe(&self) -> Vec<String> {
        let fmt_pid = |p: Option<u32>, absent: &str| {
            p.map(|p| p.to_string())
                .unwrap_or_else(|| absent.to_string())
        };
        let mut lines = vec![
            format!(
                "State file PID: {}",
                fmt_pid(self.state_pid, "none (no PID file, or stale)")
            ),
            format!(
                "Port occupant PID: {}",
                fmt_pid(self.port_pid, "none (nothing is listening)")
            ),
        ];
        if let Some(name) = self.manager_name {
            lines.push(format!(
                "{name} unit owns PID: {}",
                fmt_pid(
                    self.manager_pid,
                    "none (unit installed but not currently running a process)"
                )
            ));
        }
        if self.is_orphan {
            let orphan_pid = self.port_pid.expect("is_orphan implies port_pid is Some");
            let manager = self.manager_name.unwrap_or("the service manager");
            lines.push(format!(
                "Orphan: PID {orphan_pid} holds the port but is not managed by {manager} — stop the orphan (`canopy daemon stop`) so the managed service can take the port."
            ));
        }
        lines
    }
}

/// Compare PID-file, port-occupant, and service-manager facts and decide
/// whether the daemon is cleanly stopped, cleanly running, or in a state
/// worth flagging. Pure function — the I/O to gather `state_pid`,
/// `port_pid`, and `manager` lives in callers so this stays trivially
/// testable.
pub(crate) fn diagnose_daemon(
    state_pid: Option<u32>,
    port_pid: Option<u32>,
    manager: Option<&ServiceManagerFacts>,
) -> DaemonState {
    if state_pid.is_none() && port_pid.is_none() {
        return DaemonState::Stopped;
    }

    let manager_pid = manager.and_then(|m| m.pid);
    let manager_agrees = match manager {
        Some(_) => manager_pid == port_pid,
        None => true,
    };
    let all_agree =
        state_pid.is_some() && port_pid.is_some() && state_pid == port_pid && manager_agrees;

    if all_agree {
        return DaemonState::Running {
            pid: port_pid.expect("checked above"),
        };
    }

    let is_orphan = port_pid.is_some() && manager.is_some() && manager_pid != port_pid;

    DaemonState::Discrepancy(DaemonDiscrepancy {
        state_pid,
        port_pid,
        manager_name: manager.map(|m| m.name),
        manager_pid,
        is_orphan,
    })
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
    fn other_instance_may_be_running_true_when_pid_file_names_live_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("daemon.pid"),
            std::process::id().to_string(),
        )
        .expect("write pid");
        assert!(other_instance_may_be_running(dir.path()));
    }

    #[test]
    fn other_instance_may_be_running_false_when_no_pid_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!other_instance_may_be_running(dir.path()));
    }

    #[cfg(unix)]
    #[test]
    fn other_instance_may_be_running_false_for_stale_pid_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("daemon.pid"), "999999999").expect("write pid");
        assert!(!other_instance_may_be_running(dir.path()));
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

    // ── resolve_port_pid (Linux /proc parsing) ────────────────────────

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_port_pid_finds_current_process_bound_to_an_ephemeral_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();

        let found = resolve_port_pid(port);

        assert_eq!(found, Some(std::process::id()));
        drop(listener);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_port_pid_returns_none_for_a_port_nothing_listens_on() {
        // Bind to grab a genuinely free ephemeral port, then release it —
        // vanishingly unlikely to be immediately reused within the test.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        assert_eq!(resolve_port_pid(port), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_listening_inode_matches_listen_state_and_port() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1E43 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 123456 1 0000000000000000 100 0 0 10 0\n";
        assert_eq!(parse_listening_inode(table, 0x1E43), Some(123456));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_listening_inode_ignores_non_listen_states() {
        // st = 01 (ESTABLISHED), not 0A (LISTEN) — must not match even
        // though the port and inode look right.
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1E43 00000000:0000 01 00000000:00000000 00:00000000 00000000  1000        0 123456 1 0000000000000000 100 0 0 10 0\n";
        assert_eq!(parse_listening_inode(table, 0x1E43), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_listening_inode_ignores_other_ports() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1E43 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 123456 1 0000000000000000 100 0 0 10 0\n";
        assert_eq!(parse_listening_inode(table, 0x9999), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_listening_inode_handles_empty_table() {
        let header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        assert_eq!(parse_listening_inode(header, 7755), None);
    }

    // ── is_unit_cgroup (systemd cgroup matching) ───────────────────────

    #[test]
    fn is_unit_cgroup_matches_exact_trailing_segment() {
        let content = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/canopy.service\n";
        assert!(is_unit_cgroup(content, "canopy.service"));
    }

    #[test]
    fn is_unit_cgroup_rejects_substring_matches() {
        let content =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/notcanopy.service\n";
        assert!(!is_unit_cgroup(content, "canopy.service"));
    }

    #[test]
    fn is_unit_cgroup_rejects_prefix_matches() {
        let content =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/canopy.service.old\n";
        assert!(!is_unit_cgroup(content, "canopy.service"));
    }

    #[test]
    fn is_unit_cgroup_matches_across_multiple_hierarchy_lines() {
        // cgroup v1 multi-line format: only one line needs to match.
        let content = "12:pids:/user.slice/user-1000.slice\n1:name=systemd:/user.slice/user-1000.slice/user@1000.service/app.slice/canopy.service\n";
        assert!(is_unit_cgroup(content, "canopy.service"));
    }

    #[test]
    fn is_unit_cgroup_false_on_empty_content() {
        assert!(!is_unit_cgroup("", "canopy.service"));
    }

    // ── parse_launchctl_pid (launchd list output) ──────────────────────

    #[test]
    fn parse_launchctl_pid_extracts_pid_from_plist_style_output() {
        let text = "{\n\t\"PID\" = 4242;\n\t\"LastExitStatus\" = 0;\n};\n";
        assert_eq!(parse_launchctl_pid(text), Some(4242));
    }

    #[test]
    fn parse_launchctl_pid_returns_none_when_job_not_loaded() {
        let text = "{\n\t\"LastExitStatus\" = 0;\n};\n";
        assert_eq!(parse_launchctl_pid(text), None);
    }

    #[test]
    fn parse_launchctl_pid_returns_none_for_empty_input() {
        assert_eq!(parse_launchctl_pid(""), None);
    }

    // ── diagnose_daemon (the core truth table) ─────────────────────────

    #[test]
    fn diagnose_reports_stopped_when_nothing_is_alive_and_nothing_holds_the_port() {
        assert_eq!(diagnose_daemon(None, None, None), DaemonState::Stopped);
    }

    #[test]
    fn diagnose_reports_running_when_state_and_port_agree_with_no_manager() {
        assert_eq!(
            diagnose_daemon(Some(42), Some(42), None),
            DaemonState::Running { pid: 42 }
        );
    }

    #[test]
    fn diagnose_reports_running_when_all_three_facts_agree() {
        let manager = ServiceManagerFacts {
            name: "systemd",
            pid: Some(42),
        };
        assert_eq!(
            diagnose_daemon(Some(42), Some(42), Some(&manager)),
            DaemonState::Running { pid: 42 }
        );
    }

    #[test]
    fn diagnose_flags_the_bug_scenario_as_orphan() {
        // The exact reported bug: PID file and port agree on the orphan,
        // but the systemd unit owns no process at all (failing to bind,
        // MainPID=0). Must not read as plain RUNNING.
        let manager = ServiceManagerFacts {
            name: "systemd",
            pid: None,
        };
        let state = diagnose_daemon(Some(9999), Some(9999), Some(&manager));
        match state {
            DaemonState::Discrepancy(d) => {
                assert!(d.is_orphan);
                assert_eq!(d.state_pid, Some(9999));
                assert_eq!(d.port_pid, Some(9999));
                assert_eq!(d.manager_pid, None);
                assert_eq!(d.manager_name, Some("systemd"));
            }
            other => panic!("expected Discrepancy, got {other:?}"),
        }
    }

    #[test]
    fn diagnose_flags_orphan_when_manager_owns_a_different_pid() {
        // A second, unmanaged process holds the port while systemd's own
        // (different) instance is still alive — impossible on a real port,
        // but the diagnostic must not silently pick one.
        let manager = ServiceManagerFacts {
            name: "systemd",
            pid: Some(111),
        };
        let state = diagnose_daemon(Some(222), Some(222), Some(&manager));
        match state {
            DaemonState::Discrepancy(d) => {
                assert!(d.is_orphan);
                assert_eq!(d.manager_pid, Some(111));
                assert_eq!(d.port_pid, Some(222));
            }
            other => panic!("expected Discrepancy, got {other:?}"),
        }
    }

    #[test]
    fn diagnose_flags_discrepancy_without_orphan_when_no_manager_installed() {
        // State file and port disagree (stale PID file pointing at a dead
        // PID while something else now holds the port) but there's no
        // service manager in the picture, so this isn't the orphan case —
        // just a plain inconsistency.
        let state = diagnose_daemon(Some(1), Some(2), None);
        match state {
            DaemonState::Discrepancy(d) => {
                assert!(!d.is_orphan);
                assert_eq!(d.manager_name, None);
            }
            other => panic!("expected Discrepancy, got {other:?}"),
        }
    }

    #[test]
    fn diagnose_flags_discrepancy_when_state_pid_missing_but_port_held() {
        // No PID file (or a stale one already filtered to None by the
        // caller) but something live holds the port — must still surface
        // as a discrepancy, not STOPPED.
        let state = diagnose_daemon(None, Some(5), None);
        match state {
            DaemonState::Discrepancy(d) => {
                assert_eq!(d.state_pid, None);
                assert_eq!(d.port_pid, Some(5));
            }
            other => panic!("expected Discrepancy, got {other:?}"),
        }
    }

    #[test]
    fn diagnose_flags_discrepancy_when_manager_installed_but_nothing_holds_port() {
        // Unit installed, PID file alive, but nothing is actually listening
        // — the PID file lied.
        let manager = ServiceManagerFacts {
            name: "systemd",
            pid: None,
        };
        let state = diagnose_daemon(Some(7), None, Some(&manager));
        match state {
            DaemonState::Discrepancy(d) => {
                assert!(!d.is_orphan, "no port occupant means no orphan to name");
                assert_eq!(d.state_pid, Some(7));
                assert_eq!(d.port_pid, None);
            }
            other => panic!("expected Discrepancy, got {other:?}"),
        }
    }

    #[test]
    fn discrepancy_describe_names_both_pids_and_the_remedy_for_the_orphan_case() {
        let d = DaemonDiscrepancy {
            state_pid: Some(9999),
            port_pid: Some(9999),
            manager_name: Some("systemd"),
            manager_pid: None,
            is_orphan: true,
        };
        let lines = d.describe();
        let joined = lines.join("\n");
        assert!(
            joined.contains("9999"),
            "must name the orphan PID:\n{joined}"
        );
        assert!(
            joined.contains("systemd unit owns PID: none"),
            "must name the manager's empty MainPID:\n{joined}"
        );
        assert!(
            joined.contains("canopy daemon stop"),
            "must give the one-line remedy:\n{joined}"
        );
    }

    #[test]
    fn discrepancy_describe_omits_manager_line_when_no_manager_installed() {
        let d = DaemonDiscrepancy {
            state_pid: Some(1),
            port_pid: Some(2),
            manager_name: None,
            manager_pid: None,
            is_orphan: false,
        };
        let joined = d.describe().join("\n");
        assert!(!joined.contains("unit owns"));
        assert!(!joined.contains("Orphan:"));
    }
}
