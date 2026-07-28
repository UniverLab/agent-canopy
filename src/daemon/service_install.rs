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
    let exe_str = exe.display().to_string();

    // Capture PATH from the process environment rather than shelling out to a
    // login shell. This is the correct choice because `canopy daemon install` is
    // run by the same user who installed their CLIs, and the process already
    // inherits that user's PATH. Shelling out to `bash -lc 'echo $PATH'` would
    // risk picking up a different shell profile (or failing in headless/SSH
    // environments). The daemon's PATH is written once at install time and
    // kept up to date by re-running `canopy daemon install`.
    let current_path = std::env::var("PATH").unwrap_or_default();
    let path_value = reconcile_path(unit_path.as_path(), &current_path);

    let unit_content = render_unit_content(&exe_str, port, &path_value);

    std::fs::write(&unit_path, unit_content)?;
    println!("Created {}", unit_path.display());

    ensure_linger_enabled();
    reload_and_enable_service()?;

    Ok(())
}

/// Render the systemd unit file contents.
///
/// Includes `Environment=PATH=` so CLI binaries installed under a user's
/// home directory (e.g. `~/.opencode/bin`) resolve under the daemon's
/// minimal systemd PATH, not just when the daemon inherits a shell's PATH.
fn render_unit_content(exe_str: &str, port: u16, path_value: &str) -> String {
    format!(
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
Environment=PATH={path_value}

[Install]
WantedBy=default.target
"#
    )
}

/// Compute the `PATH` to write into the unit, preserving any custom entries
/// from an existing unit's `Environment=PATH=` line that aren't present in
/// `new_path`.
///
/// Reinstalling used to silently overwrite the whole unit file, wiping out
/// any hand-added `Environment=PATH=` (e.g. one including `~/.opencode/bin`
/// or `~/.grok/bin`). Rather than clobber it again, entries that would be
/// lost are appended to the new PATH and reported on stdout.
fn reconcile_path(unit_path: &std::path::Path, new_path: &str) -> String {
    let Ok(existing) = std::fs::read_to_string(unit_path) else {
        return new_path.to_string();
    };

    let Some(old_path) = existing
        .lines()
        .find_map(|line| line.strip_prefix("Environment=PATH="))
    else {
        return new_path.to_string();
    };

    if old_path == new_path {
        return new_path.to_string();
    }

    let new_entries: std::collections::HashSet<&str> = new_path.split(':').collect();
    let preserved: Vec<&str> = old_path
        .split(':')
        .filter(|entry| !entry.is_empty() && !new_entries.contains(entry))
        .collect();

    if preserved.is_empty() {
        return new_path.to_string();
    }

    println!(
        "  Existing {} has PATH entries not in the new PATH: {}",
        SYSTEMD_SERVICE_NAME,
        preserved.join(":")
    );
    println!("  Keeping them appended so previously working CLIs don't break.");

    let mut merged = new_path.to_string();
    for entry in preserved {
        merged.push(':');
        merged.push_str(entry);
    }
    merged
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Does not touch the user's real `~/.config/systemd/user/canopy.service`
    /// — exercises the pure content-generation function directly.
    #[test]
    fn unit_template_includes_nonempty_path_environment() {
        let content = render_unit_content("/usr/bin/canopy", 4177, "/usr/bin:/bin");

        let path_line = content
            .lines()
            .find(|line| line.starts_with("Environment=PATH="))
            .expect("unit must declare Environment=PATH=");
        let value = path_line.trim_start_matches("Environment=PATH=");

        assert!(!value.is_empty());
    }

    #[test]
    fn reconcile_path_returns_new_path_when_no_existing_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let unit_path = tmp.path().join("canopy.service");

        let result = reconcile_path(&unit_path, "/usr/bin:/bin");

        assert_eq!(result, "/usr/bin:/bin");
    }

    #[test]
    fn reconcile_path_preserves_custom_entries_missing_from_new_path() {
        let tmp = tempfile::tempdir().unwrap();
        let unit_path = tmp.path().join("canopy.service");
        std::fs::write(
            &unit_path,
            "[Service]\nEnvironment=PATH=/home/u/.opencode/bin:/usr/bin:/bin\n",
        )
        .unwrap();

        let result = reconcile_path(&unit_path, "/usr/bin:/bin");

        assert!(result.contains("/home/u/.opencode/bin"));
        assert!(result.contains("/usr/bin"));
        assert!(result.contains("/bin"));
    }

    #[test]
    fn render_unit_content_includes_exe_and_port() {
        let content = render_unit_content("/usr/bin/canopy", 7755, "/usr/bin:/bin");
        assert!(content.contains("/usr/bin/canopy"));
        assert!(content.contains("7755"));
        assert!(content.contains("[Unit]"));
        assert!(content.contains("[Service]"));
        assert!(content.contains("[Install]"));
        assert!(content.contains("ExecStart="));
        assert!(content.contains("Environment=PATH="));
    }

    #[test]
    fn render_unit_content_uses_custom_port() {
        let content = render_unit_content("/usr/bin/canopy", 9999, "/usr/bin");
        assert!(content.contains("9999"));
        assert!(!content.contains("7755"));
    }

    #[test]
    fn render_unit_content_uses_custom_path() {
        let content = render_unit_content("/usr/bin/canopy", 7755, "/custom/path:/another/path");
        assert!(content.contains("/custom/path:/another/path"));
    }
}
