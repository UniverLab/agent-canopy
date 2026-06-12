use anyhow::Result;

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
            for line in text.lines() {
                if let Some(pid_start) = line.find("pid=") {
                    let rest = &line[pid_start + 4..];
                    if let Some(end) = rest.find(|c: char| !c.is_ascii_digit()) {
                        if let Ok(pid) = rest[..end].parse::<u32>() {
                            let self_pid = std::process::id();
                            if pid != self_pid && pid != 0 {
                                eprintln!("Port {port} occupied by PID {pid} — sending SIGTERM");
                                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                if unsafe { libc::kill(pid as i32, 0) } == 0 {
                                    eprintln!("PID {pid} did not exit — sending SIGKILL");
                                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                                    std::thread::sleep(std::time::Duration::from_millis(200));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = port;
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
