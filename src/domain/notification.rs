//! System notifications — cross-platform desktop notifications.
//!
//! Sends native notifications when agents complete or fail.
//! Detected platforms: WSL → Windows toast, macOS → osascript, Linux → notify-send.
//! All notifications are fire-and-forget on a background thread.
//!
//! ## Windows AUMID Registration
//!
//! Windows requires an AppUserModelId (AUMID) to be registered in the current
//! user's registry before `ToastNotificationManager::History` can resolve it.
//! Without registration, `GetHistory()` returns `0x80070490`.
//!
//! `register_aumid()` is called once at startup (WSL only) to write:
//!   `Registry::HKEY_CURRENT_USER\Software\Classes\AppUserModelId\Canopy`
//! with `DisplayName` and optional `IconUri`.

use std::process::Command;

/// Canonical AppUserModelId for Canopy toast notifications.
/// Must match exactly between registry key name and `CreateToastNotifier()` calls.
const APP_ID: &str = "Canopy";

/// Severity of a notification. Drives the native themed icon and urgency on
/// platforms that support them (Linux). On WSL/macOS the platform shows the
/// Canopy app icon regardless, so the level is informational only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl NotificationLevel {
    /// Freedesktop standard (themed) icon name — rendered natively by the
    /// desktop notification daemon, no bundled assets required.
    fn icon_name(self) -> &'static str {
        match self {
            NotificationLevel::Info => "dialog-information",
            NotificationLevel::Success => "emblem-default",
            NotificationLevel::Warning => "dialog-warning",
            NotificationLevel::Error => "dialog-error",
        }
    }

    fn is_critical(self) -> bool {
        matches!(self, NotificationLevel::Error)
    }
}

/// Detected runtime platform for notification dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Wsl,
    MacOs,
    Linux,
}

fn detect_platform() -> Platform {
    if cfg!(target_os = "macos") {
        return Platform::MacOs;
    }

    // WSL: /proc/version contains "microsoft" or "Microsoft"
    if let Ok(ver) = std::fs::read_to_string("/proc/version") {
        if ver.to_lowercase().contains("microsoft") {
            return Platform::Wsl;
        }
    }

    Platform::Linux
}

/// Escape a string for use inside a PowerShell single-quoted string.
fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Escape a string for use inside an AppleScript double-quoted string.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ── Windows AUMID Registration ───────────────────────────────────────

/// Register the Canopy AppUserModelId in the Windows registry so that
/// `ToastNotificationManager::History` can resolve it without `0x80070490`.
///
/// Writes to `HKCU:\Software\Classes\AppUserModelId\Canopy` with:
///   - `DisplayName` = "Canopy"
///   - `IconUri`     = path to the current executable (best-effort)
///
/// Uses the `Registry::` provider path to avoid accidentally creating a
/// filesystem directory (`HKCU/`) in the current working directory.
/// Safe to call multiple times — overwrites existing values idempotently.
/// Only runs on WSL; no-op on other platforms.
pub fn register_aumid() {
    if detect_platform() != Platform::Wsl {
        return;
    }
    std::thread::spawn(|| {
        let icon_uri = std::env::current_exe()
            .ok()
            .map(|p| ps_escape(&p.to_string_lossy()))
            .unwrap_or_default();

        // Use full Registry:: provider path to guarantee PowerShell targets
        // the Windows registry, never the filesystem.  `-Path` uses the
        // provider-qualified form so there is no ambiguity regardless of
        // the current working directory or PSDrive availability.
        let script = format!(
            concat!(
                "$key = 'Registry::HKEY_CURRENT_USER\\Software\\Classes\\AppUserModelId\\{}'; ",
                "New-Item -Path $key -Force | Out-Null; ",
                "New-ItemProperty -Path $key -Name 'DisplayName' -Value '{}' ",
                "-PropertyType String -Force | Out-Null; ",
                "New-ItemProperty -Path $key -Name 'IconUri' -Value '{}' ",
                "-PropertyType String -Force | Out-Null",
            ),
            APP_ID, APP_ID, icon_uri,
        );
        let _ = Command::new("powershell.exe")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(&script)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

// ── Platform senders ─────────────────────────────────────────────────

fn send_linux(title: &str, body: &str, level: NotificationLevel) {
    let mut cmd = Command::new("notify-send");
    cmd.arg("--app-name=Canopy")
        .arg(format!("--icon={}", level.icon_name()));
    if level.is_critical() {
        // Critical alerts stay on screen until dismissed on most daemons.
        cmd.arg("--urgency=critical");
    }
    let _ = cmd
        .arg(title)
        .arg(body)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn send_macos(title: &str, body: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(body),
        applescript_escape(title),
    );
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn send_wsl(title: &str, body: &str) {
    // Clear stale notifications and show the new toast in a single PowerShell process
    // to avoid a race condition where two spawned processes interfere with each other.
    let ps_script = format!(
        concat!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ",
            "ContentType = WindowsRuntime] > $null; ",
            "try {{ [Windows.UI.Notifications.ToastNotificationManager]::",
            "CreateToastNotifier('{}').Clear() }} catch {{}}; ",
            "$template = [Windows.UI.Notifications.ToastNotificationManager]::",
            "GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); ",
            "$nodes = $template.GetElementsByTagName('text'); ",
            "$nodes.Item(0).AppendChild($template.CreateTextNode('{}')) > $null; ",
            "$nodes.Item(1).AppendChild($template.CreateTextNode('{}')) > $null; ",
            "$toast = [Windows.UI.Notifications.ToastNotification]::new($template); ",
            "$toast.ExpirationTime = [DateTimeOffset]::UtcNow.Add([TimeSpan]::FromSeconds(30)); ",
            "[Windows.UI.Notifications.ToastNotificationManager]::",
            "CreateToastNotifier('{}').Show($toast)"
        ),
        ps_escape(APP_ID),
        ps_escape(title),
        ps_escape(body),
        ps_escape(APP_ID),
    );
    let _ = Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(&ps_script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ── Public API ───────────────────────────────────────────────────────

/// Clear any stale Canopy notifications from the Windows Action Center.
/// Call this once at startup to prevent pile-up of old notifications.
/// Only runs on WSL; no-op on other platforms.
pub fn clear_stale_notifications() {
    if detect_platform() != Platform::Wsl {
        return;
    }
    std::thread::spawn(|| {
        let clear_script = format!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, \
             ContentType = WindowsRuntime] > $null; \
             try {{ [Windows.UI.Notifications.ToastNotificationManager]::\
             CreateToastNotifier('{}').Clear() }} catch {{}}",
            ps_escape(APP_ID),
        );
        let _ = Command::new("powershell.exe")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(&clear_script)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

/// Clear all Canopy notifications from the Windows Action Center.
/// Call this on app exit / task cancellation to avoid stale notifications
/// lingering in the Action Center after the process terminates.
///
/// Unlike `clear_stale_notifications`, this blocks until the PowerShell
/// process completes so the cleanup is guaranteed before the process exits.
/// Only runs on WSL; no-op on other platforms.
pub fn clear_notifications_on_exit() {
    if detect_platform() != Platform::Wsl {
        return;
    }
    let clear_script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, \
         ContentType = WindowsRuntime] > $null; \
         try {{ [Windows.UI.Notifications.ToastNotificationManager]::\
         CreateToastNotifier('{}').Clear() }} catch {{}}",
        ps_escape(APP_ID),
    );
    let _ = Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(&clear_script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Send a desktop notification. Fire-and-forget — spawns a background thread
/// and never blocks the caller. Failures are silently ignored.
///
/// `level` selects a native themed icon and urgency on Linux. On WSL and macOS
/// the system shows the Canopy app icon, so the level has no visible effect
/// there — the title (shown prominently) carries the subject and the body the
/// outcome.
pub fn send_notification(title: &str, body: &str, level: NotificationLevel) {
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || match detect_platform() {
        Platform::Wsl => send_wsl(&title, &body),
        Platform::MacOs => send_macos(&title, &body),
        Platform::Linux => send_linux(&title, &body, level),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_platform_returns_valid_enum() {
        let platform = detect_platform();
        assert!(
            matches!(platform, Platform::Wsl | Platform::MacOs | Platform::Linux),
            "Platform should be one of the three variants"
        );
    }

    #[test]
    fn test_ps_escape_single_quotes() {
        assert_eq!(ps_escape("hello"), "hello");
        assert_eq!(ps_escape("it's"), "it''s");
        assert_eq!(ps_escape("don't"), "don''t");
        assert_eq!(ps_escape("''"), "''''");
    }

    #[test]
    fn test_ps_escape_empty() {
        assert_eq!(ps_escape(""), "");
    }

    #[test]
    fn test_applescript_escape_backslash() {
        assert_eq!(applescript_escape("hello"), "hello");
        assert_eq!(applescript_escape("hello\\world"), "hello\\\\world");
        assert_eq!(applescript_escape("a\\b\\c"), "a\\\\b\\\\c");
    }

    #[test]
    fn test_applescript_escape_quotes() {
        assert_eq!(applescript_escape("say \"hello\""), "say \\\"hello\\\"");
        assert_eq!(applescript_escape("it's"), "it's");
    }

    #[test]
    fn test_applescript_escape_empty() {
        assert_eq!(applescript_escape(""), "");
    }

    #[test]
    fn test_applescript_escape_combined() {
        assert_eq!(
            applescript_escape("say \"hello\\world\""),
            "say \\\"hello\\\\world\\\""
        );
    }
}
