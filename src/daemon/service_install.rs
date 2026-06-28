//! System service installation and uninstallation.
//!
//! Supports:
//! - **Linux/WSL**: systemd user unit at `~/.config/systemd/user/canopy.service`
//! - **macOS**: launchd agent at `~/Library/LaunchAgents/com.canopy.plist`

use anyhow::Result;

/// Install the daemon as a system service that starts on boot.
pub fn install_service(exe_path: &std::path::Path, port: u16) -> Result<()> {
    let exe = exe_path
        .canonicalize()
        .unwrap_or_else(|_| exe_path.to_path_buf());

    if cfg!(target_os = "macos") {
        install_launchd_service(&exe, port)
    } else if cfg!(target_os = "linux") {
        install_systemd_service(&exe, port)
    } else {
        anyhow::bail!(
            "Service installation is not supported on this platform (only Linux and macOS)"
        )
    }
}

/// Uninstall the system service.
pub fn uninstall_service() -> Result<()> {
    if cfg!(target_os = "macos") {
        uninstall_launchd_service()
    } else if cfg!(target_os = "linux") {
        uninstall_systemd_service()
    } else {
        anyhow::bail!("Service uninstallation is not supported on this platform")
    }
}

// -- systemd (Linux/WSL) ------------------------------------------------------

const SYSTEMD_SERVICE_NAME: &str = "canopy.service";

fn systemd_unit_dir() -> Result<std::path::PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home.join(".config/systemd/user"))
}

fn install_systemd_service(exe: &std::path::Path, port: u16) -> Result<()> {
    let unit_dir = systemd_unit_dir()?;
    std::fs::create_dir_all(&unit_dir)?;

    let unit_path = unit_dir.join(SYSTEMD_SERVICE_NAME);
    let exe_str = exe.display();

    let unit_content = format!(
        r#"[Unit]
Description=canopy daemon
After=network.target

[Service]
Type=simple
ExecStart={exe_str} serve --port {port}
Restart=on-failure
RestartSec=5
StartLimitIntervalSec=60
StartLimitBurst=5
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#
    );

    std::fs::write(&unit_path, unit_content)?;
    println!("Created {}", unit_path.display());

    ensure_linger_enabled();
    reload_and_enable_service()?;

    Ok(())
}

fn ensure_linger_enabled() {
    let Some(user) = std::env::var("USER").ok() else {
        return;
    };

    let linger_enabled = std::process::Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger"])
        .output()
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "Linger=yes")
        .unwrap_or(false);

    if linger_enabled {
        return;
    }

    match std::process::Command::new("loginctl")
        .args(["enable-linger", &user])
        .status()
    {
        Ok(s) if s.success() => {
            println!("  Lingering enabled (service survives logout/reboot)");
        }
        _ => {
            println!("  ⚠ Could not enable lingering — service may stop on logout/reboot.");
            println!("    Run manually: sudo loginctl enable-linger {user}");
        }
    }
}

fn reload_and_enable_service() -> Result<()> {
    match std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
    {
        Ok(s) if s.success() => {}
        _ => {
            println!("  ⚠ systemctl daemon-reload failed (systemd may not be fully available)");
            println!("    The unit file has been written — you can enable it manually:");
            println!("    systemctl --user enable --now {SYSTEMD_SERVICE_NAME}");
            return Ok(());
        }
    }

    match std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", SYSTEMD_SERVICE_NAME])
        .status()
    {
        Ok(s) if s.success() => {
            println!("  Service enabled and started");
            println!("    Check status: systemctl --user status {SYSTEMD_SERVICE_NAME}");
            println!("    View logs:    journalctl --user -u {SYSTEMD_SERVICE_NAME} -f");
        }
        _ => {
            println!("  ⚠ Failed to enable service automatically");
            println!("    Enable manually: systemctl --user enable --now {SYSTEMD_SERVICE_NAME}");
        }
    }

    Ok(())
}

#[allow(dead_code)]
fn uninstall_systemd_service() -> Result<()> {
    let unit_dir = systemd_unit_dir()?;
    let unit_path = unit_dir.join(SYSTEMD_SERVICE_NAME);

    if !unit_path.exists() {
        println!("Service is not installed (no unit file found)");
        return Ok(());
    }

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "stop", SYSTEMD_SERVICE_NAME])
        .status();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", SYSTEMD_SERVICE_NAME])
        .status();

    std::fs::remove_file(&unit_path)?;
    println!("Removed {}", unit_path.display());

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    println!("Service stopped and uninstalled");
    Ok(())
}

// -- launchd (macOS) ----------------------------------------------------------

const LAUNCHD_LABEL: &str = "com.canopy";

fn launchd_plist_path() -> Result<std::path::PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")))
}

fn install_launchd_service(exe: &std::path::Path, port: u16) -> Result<()> {
    let plist_path = launchd_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let log_dir = home.join(".canopy");
    std::fs::create_dir_all(&log_dir)?;

    let exe_str = exe.display();
    let stdout_log = log_dir.join("daemon.log").display().to_string();
    let stderr_log = log_dir.join("daemon.err.log").display().to_string();

    let plist_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_str}</string>
        <string>serve</string>
        <string>--port</string>
        <string>{port}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{stdout_log}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_log}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>
"#
    );

    if plist_path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.display().to_string()])
            .status();
    }

    std::fs::write(&plist_path, plist_content)?;
    println!("Created {}", plist_path.display());

    let load = std::process::Command::new("launchctl")
        .args(["load", &plist_path.display().to_string()])
        .status()?;

    if load.success() {
        println!("Service loaded and started");
        println!("  Check status: launchctl list | grep {LAUNCHD_LABEL}");
        println!("  View logs:    tail -f {stdout_log}");
    } else {
        println!("Warning: launchctl load failed");
        println!("  Try manually: launchctl load {}", plist_path.display());
    }

    Ok(())
}

#[allow(dead_code)]
fn uninstall_launchd_service() -> Result<()> {
    let plist_path = launchd_plist_path()?;

    if !plist_path.exists() {
        println!("Service is not installed (no plist found)");
        return Ok(());
    }

    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.display().to_string()])
        .status();

    std::fs::remove_file(&plist_path)?;
    println!("Removed {}", plist_path.display());
    println!("Service stopped and uninstalled");
    Ok(())
}
