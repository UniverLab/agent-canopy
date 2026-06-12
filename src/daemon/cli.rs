use anyhow::Result;

use clap::Subcommand;

use crate::application::ports::{AgentRepository, StateRepository};
use crate::daemon::process::{
    is_process_running, kill_port_occupant, print_last_n_lines, read_pid, remove_pid_file,
    send_signal,
};

#[cfg(target_os = "linux")]
use crate::daemon::process::{is_service_enabled, is_systemd_available};

use crate::daemon::service_install;
use crate::db::Database;

#[derive(Subcommand)]
pub(crate) enum DaemonAction {
    Start,
    Stop,
    Status,
    Restart,
    Logs,
    InstallService,
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
    let Some(pid) = read_pid(data_dir) else {
        println!("Daemon is not running (no PID file)");
        return Ok(());
    };

    if !is_process_running(pid) {
        println!("Daemon is not running (stale PID file)");
        remove_pid_file(data_dir);
        return Ok(());
    }

    send_signal(pid);
    println!("Sent stop signal to daemon (PID: {pid})");

    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if !is_process_running(pid) {
            break;
        }
    }

    remove_pid_file(data_dir);

    if is_process_running(pid) {
        eprintln!("Warning: daemon (PID: {pid}) did not stop within 5 seconds");
    } else {
        println!("Daemon stopped");
    }

    Ok(())
}

fn handle_status(data_dir: &std::path::Path) -> Result<()> {
    let pid_info = read_pid(data_dir);

    if !pid_info.map(is_process_running).unwrap_or(false) {
        println!("Daemon: STOPPED");
        if pid_info.is_some() {
            remove_pid_file(data_dir);
        }
        return Ok(());
    }

    let pid = pid_info.expect("pid checked above");

    let Ok(db) = Database::new(&data_dir.join("background_agents.db")) else {
        println!("Daemon: RUNNING (PID: {pid})");
        return Ok(());
    };

    let port = db.get_state("port")?.unwrap_or_else(|| "7755".to_string());
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
