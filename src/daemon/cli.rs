use anyhow::Result;

use clap::Subcommand;

use crate::application::ports::{AgentRepository, StateRepository};
use crate::daemon::process::{
    diagnose_daemon, is_process_running, kill_port_occupant, print_last_n_lines, read_pid,
    remove_pid_file, resolve_port_pid, send_signal, service_manager_facts, DaemonState,
};

#[cfg(target_os = "linux")]
use crate::daemon::process::{is_service_enabled, is_systemd_available};

use crate::daemon::service_install;
use crate::db::Database;

#[derive(Subcommand)]
pub(crate) enum DaemonAction {
    /// Start the daemon in the background.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Show daemon process status and agent counts.
    Status,
    /// Restart the daemon (stop then start).
    Restart,
    /// Print the last 50 lines of daemon logs.
    Logs,
    /// Install the daemon as a system service.
    InstallService,
    /// Remove the daemon system service.
    UninstallService,
}

pub(crate) async fn handle_daemon_action(
    action: DaemonAction,
    port_override: Option<u16>,
) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;

    match action {
        DaemonAction::Start => handle_start(&data_dir, port_override).await,
        DaemonAction::Stop => handle_stop(&data_dir).await,
        DaemonAction::Status => handle_status(&data_dir),
        DaemonAction::Restart => handle_restart(port_override).await,
        DaemonAction::Logs => handle_logs(&data_dir),
        DaemonAction::InstallService => handle_install_service(port_override),
        DaemonAction::UninstallService => handle_uninstall_service(),
    }
}

/// The port a running daemon last recorded itself on, falling back to
/// `resolve_port(None)` (env var or default) when the database doesn't
/// exist yet or has no `port` state — the same fallback `handle_status`
/// always used before it also needed this to check who holds the port.
///
/// Gated on the db file already existing: `Database::new` creates and
/// seeds a fresh `background_agents.db` as a side effect when the path is
/// missing, and a read-only status/stop check must not do that on a
/// machine that has never started the daemon.
fn configured_port(data_dir: &std::path::Path) -> u16 {
    let db_path = data_dir.join("background_agents.db");
    db_path
        .exists()
        .then(|| Database::new(&db_path).ok())
        .flatten()
        .and_then(|db| db.get_state("port").ok().flatten())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| crate::resolve_port(None))
}

async fn handle_start(data_dir: &std::path::Path, port_override: Option<u16>) -> Result<()> {
    if let Some(pid) = read_pid(data_dir) {
        if is_process_running(pid) {
            println!("Daemon is already running (PID: {pid})");
            return Ok(());
        }
        remove_pid_file(data_dir);
    }

    let exe = std::env::current_exe()?;
    let port = crate::resolve_port(port_override);

    // The PID-file check above can miss a daemon that's alive and well but
    // whose PID this invocation doesn't know about yet — e.g. a stale or
    // missing PID file right after `canopy daemon install`. Killing
    // whatever holds the port unconditionally and forking a fresh detached
    // `canopy serve` on top of it is exactly how an untracked orphan that
    // the unit can never supersede gets created: the fork wins the race for
    // the port, and every subsequent restart of the managed unit fails to
    // bind. If the current occupant is already the service manager's own
    // process, leave it alone instead of replacing it.
    if let Some(manager) = service_manager_facts() {
        if let Some(pid) = resolve_port_pid(port) {
            if manager.pid == Some(pid) {
                println!(
                    "Daemon is already running under {} (PID: {pid})",
                    manager.name
                );
                return Ok(());
            }
        }
    }

    kill_port_occupant(port);

    install_service_if_needed(&exe, port);

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve");
    if let Some(p) = port_override {
        cmd.arg("--port").arg(p.to_string());
    }

    kill_port_occupant(port);

    let log_path = data_dir.join("daemon.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    cmd.stdout(log_file)
        .stderr(log_file_err)
        .stdin(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = cmd.spawn()?;
    let child_pid = child.id();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    if !is_process_running(child_pid) {
        eprintln!(
            "Daemon failed to start — check logs at {}",
            log_path.display()
        );
        return Err(anyhow::anyhow!("Daemon process exited immediately"));
    }

    println!("Daemon started (PID: {child_pid})");
    println!("Logs: {}", log_path.display());
    Ok(())
}

fn install_service_if_needed(_exe: &std::path::Path, _port: u16) {
    #[cfg(target_os = "linux")]
    {
        if !is_systemd_available() {
            return;
        }
        let home = dirs::home_dir().expect("No home directory");
        let service_path = home.join(".config/systemd/user/canopy.service");
        let needs_install = !service_path.exists() || !is_service_enabled();
        if !needs_install {
            return;
        }
        print!("  Installing system service... ");
        match service_install::install_service(_exe, _port) {
            Ok(_) => println!("\x1b[32m✅\x1b[0m installed"),
            Err(e) => println!("\x1b[33m⚠\x1b[0m  {}", e),
        }
    }

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().expect("No home directory");
        let plist_path = home.join("Library/LaunchAgents/com.canopy.plist");
        if plist_path.exists() {
            return;
        }
        print!("  Installing system service... ");
        match service_install::install_service(_exe, _port) {
            Ok(_) => println!("\x1b[32m✅\x1b[0m installed"),
            Err(e) => println!("\x1b[33m⚠\x1b[0m  {}", e),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (_exe, _port);
    }
}

async fn handle_stop(data_dir: &std::path::Path) -> Result<()> {
    let port = configured_port(data_dir);

    // Signal whichever PIDs are actually alive among the PID file and the
    // port's real occupant — not just the PID file. An orphaned `canopy
    // serve` that outlived its unit (or was never tracked by one) can hold
    // the port with a stale, missing, or simply different PID file; without
    // checking the port directly, `daemon stop` can never clear it and the
    // only fix is hunting the PID down by hand.
    let mut targets: Vec<u32> = Vec::new();
    if let Some(pid) = read_pid(data_dir).filter(|&p| is_process_running(p)) {
        targets.push(pid);
    }
    if let Some(pid) = resolve_port_pid(port).filter(|&p| is_process_running(p)) {
        if !targets.contains(&pid) {
            targets.push(pid);
        }
    }

    if targets.is_empty() {
        println!("Daemon is not running");
        remove_pid_file(data_dir);
        return Ok(());
    }

    for &pid in &targets {
        send_signal(pid);
    }
    let pid_list = targets
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    println!("Sent stop signal to daemon (PID: {pid_list})");

    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if targets.iter().all(|&p| !is_process_running(p)) {
            break;
        }
    }

    remove_pid_file(data_dir);

    let still_alive: Vec<u32> = targets
        .iter()
        .copied()
        .filter(|&p| is_process_running(p))
        .collect();
    if !still_alive.is_empty() {
        let list = still_alive
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("Warning: daemon (PID: {list}) did not stop within 5 seconds");
    } else {
        println!("Daemon stopped");
    }

    Ok(())
}

fn handle_status(data_dir: &std::path::Path) -> Result<()> {
    let raw_pid = read_pid(data_dir);
    let state_pid = raw_pid.filter(|&p| is_process_running(p));
    let port = configured_port(data_dir);
    let port_pid = resolve_port_pid(port);
    let manager = service_manager_facts();

    let pid = match diagnose_daemon(state_pid, port_pid, manager.as_ref()) {
        DaemonState::Stopped => {
            println!("Daemon: STOPPED");
            if raw_pid.is_some() {
                remove_pid_file(data_dir);
            }
            return Ok(());
        }
        DaemonState::Discrepancy(d) => {
            // Never print RUNNING here — a healthy version string from a
            // process that isn't the one the service manager actually owns
            // is exactly the lie this command exists to stop telling.
            println!("Daemon: INCONSISTENT");
            for line in d.describe() {
                println!("  {line}");
            }
            return Ok(());
        }
        DaemonState::Running { pid } => pid,
    };

    let Ok(db) = Database::new(&data_dir.join("background_agents.db")) else {
        println!("Daemon: RUNNING (PID: {pid})");
        return Ok(());
    };

    let version = db
        .get_state("version")?
        .unwrap_or_else(|| "unknown".to_string());
    let last_start = db
        .get_state("last_start")?
        .unwrap_or_else(|| "unknown".to_string());
    let agents = db.list_agents()?;
    let cron_count = agents.iter().filter(|a| a.is_cron()).count();
    let watch_count = agents.iter().filter(|a| a.is_watch()).count();

    println!("Daemon: RUNNING (PID: {pid})");
    println!("Version: {version}");
    println!("Port: {port}");
    println!("Started: {last_start}");
    println!(
        "Agents: {} (cron: {}, watch: {})",
        agents.len(),
        cron_count,
        watch_count
    );
    Ok(())
}

async fn handle_restart(port_override: Option<u16>) -> Result<()> {
    println!("  Restarting daemon...");
    let stop_result = Box::pin(handle_daemon_action(DaemonAction::Stop, port_override)).await;
    if let Err(e) = stop_result {
        eprintln!("Warning: stop failed: {}", e);
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    Box::pin(handle_daemon_action(DaemonAction::Start, port_override)).await
}

fn handle_logs(data_dir: &std::path::Path) -> Result<()> {
    let log_path = data_dir.join("daemon.log");
    if !log_path.exists() {
        println!("No daemon logs found at {}", log_path.display());
        return Ok(());
    }
    print_last_n_lines(&log_path, 50)
}

fn handle_install_service(port_override: Option<u16>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let port = crate::resolve_port(port_override);
    println!("Installing canopy system service...");
    match service_install::install_service(&exe, port) {
        Ok(_) => {
            println!("\x1b[32m✅\x1b[0m Service installed and enabled");
            Ok(())
        }
        Err(e) => {
            eprintln!("\x1b[31m✗\x1b[0m  Failed: {e}");
            Err(e)
        }
    }
}

fn handle_uninstall_service() -> Result<()> {
    println!("Removing canopy system service...");
    match service_install::uninstall_service() {
        Ok(_) => {
            println!("\x1b[32m✅\x1b[0m Service uninstalled");
            Ok(())
        }
        Err(e) => {
            eprintln!("\x1b[31m✗\x1b[0m  Failed: {e}");
            Err(e)
        }
    }
}
