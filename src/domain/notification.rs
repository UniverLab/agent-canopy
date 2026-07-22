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

/// Windows toast `Tag`/`Group` are capped at 64 characters starting with
/// Windows 10. Short subjects (the overwhelming majority — task ids, agent
/// ids, mission titles) pass through unchanged; anything longer is hashed to
/// a fixed-width tag so it still fits, while staying stable across repeated
/// sends of the same subject.
fn tag_for_subject(subject: &str) -> String {
    if subject.len() <= 64 {
        subject.to_string()
    } else {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        subject.hash(&mut hasher);
        format!("h{:x}", hasher.finish())
    }
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

fn send_wsl(title: &str, body: &str, tag: &str) {
    // Do NOT `.Clear()` here: clearing the notifier before every show wiped the
    // entire Canopy history from the Action Center on each toast, so an
    // unattended run of overnight events left nothing to review by morning —
    // each new toast erased all the ones before it. Startup cleanup lives in
    // `clear_stale_notifications` (called once from the TUI); individual sends
    // must be additive *across subjects*. The expiry is a full day (not 30s)
    // for the same reason: a background event fired at 2am must still be in
    // the Action Center when a human looks at 9am, rather than having
    // evaporated.
    //
    // `Tag`+`Group` scope that additivity to distinct subjects only: Windows
    // replaces (rather than adds) a toast that shares both with an existing
    // one, so re-sending the same subject (e.g. a mission re-firing, an agent
    // completing twice) collapses to a single Action Center entry instead of
    // piling up. This is the authoritative de-dup mechanism — it holds
    // regardless of how the process exits, unlike the startup/exit cleanup
    // below which is best-effort hygiene on top of it.
    let ps_script = format!(
        concat!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ",
            "ContentType = WindowsRuntime] > $null; ",
            "$template = [Windows.UI.Notifications.ToastNotificationManager]::",
            "GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); ",
            "$nodes = $template.GetElementsByTagName('text'); ",
            "$nodes.Item(0).AppendChild($template.CreateTextNode('{}')) > $null; ",
            "$nodes.Item(1).AppendChild($template.CreateTextNode('{}')) > $null; ",
            "$toast = [Windows.UI.Notifications.ToastNotification]::new($template); ",
            "$toast.Tag = '{}'; ",
            "$toast.Group = '{}'; ",
            "$toast.ExpirationTime = [DateTimeOffset]::UtcNow.Add([TimeSpan]::FromHours(24)); ",
            "[Windows.UI.Notifications.ToastNotificationManager]::",
            "CreateToastNotifier('{}').Show($toast)"
        ),
        ps_escape(title),
        ps_escape(body),
        ps_escape(tag),
        ps_escape(APP_ID),
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
///
/// This is hygiene on top of the per-subject `Tag`/`Group` de-dup in
/// `send_wsl` (the authoritative mechanism — see its comment), not a
/// substitute for it: it sweeps up entries whose subject may never be sent
/// again (e.g. a one-off status toast) and anything left over from a build
/// predating Tag/Group. Failure is logged rather than swallowed so a broken
/// startup sweep is visible instead of silently leaving a prior session's
/// pile in place.
pub fn clear_stale_notifications() {
    if detect_platform() != Platform::Wsl {
        return;
    }
    std::thread::spawn(|| {
        // Explicit try/catch + `exit 1`: an unhandled .NET exception from
        // `.Clear()` (e.g. AUMID not registered, 0x80070490) doesn't
        // reliably yield a non-zero process exit code across PowerShell
        // versions on its own, which would make failures invisible below.
        let clear_script = format!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, \
             ContentType = WindowsRuntime] > $null; \
             try {{ [Windows.UI.Notifications.ToastNotificationManager]::\
             CreateToastNotifier('{}').Clear() }} \
             catch {{ Write-Error $_.Exception.Message; exit 1 }}",
            ps_escape(APP_ID),
        );
        let result = Command::new("powershell.exe")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(&clear_script)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        match result {
            Ok(output) if output.status.success() => {
                tracing::debug!("cleared stale Canopy notifications at startup");
            }
            Ok(output) => {
                tracing::warn!(
                    "clear_stale_notifications: powershell exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(e) => {
                tracing::warn!("clear_stale_notifications: failed to spawn powershell: {e}");
            }
        }
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
///
/// On WSL, the title also doubles as the de-dup subject key (see `send_wsl`):
/// every caller already passes a stable per-subject string as `title` (a task
/// id, an agent id, a mission name, a loop name), so this needs no extra
/// plumbing per call site.
pub fn send_notification(title: &str, body: &str, level: NotificationLevel) {
    let tag = tag_for_subject(title);
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || match detect_platform() {
        Platform::Wsl => send_wsl(&title, &body, &tag),
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

    #[test]
    fn test_tag_for_subject_short_passthrough() {
        assert_eq!(tag_for_subject("First Bloom"), "First Bloom");
        assert_eq!(tag_for_subject(""), "");
    }

    #[test]
    fn test_tag_for_subject_stable_for_repeated_sends() {
        // Same subject sent repeatedly must produce the same Tag every time,
        // otherwise Windows wouldn't collapse the resends into one entry.
        let subject = "agent-xyz";
        assert_eq!(tag_for_subject(subject), tag_for_subject(subject));
    }

    #[test]
    fn test_tag_for_subject_distinct_subjects_differ() {
        assert_ne!(
            tag_for_subject("First Bloom"),
            tag_for_subject("Yolo Pilot")
        );
    }

    #[test]
    fn test_tag_for_subject_hashes_long_subjects_within_limit() {
        let long_subject = "x".repeat(200);
        let tag = tag_for_subject(&long_subject);
        assert!(tag.len() <= 64, "tag must fit Windows' 64-char limit");
        // Still stable and distinguishable from another long subject.
        let other_long_subject = "y".repeat(200);
        assert_ne!(tag, tag_for_subject(&other_long_subject));
        assert_eq!(tag, tag_for_subject(&long_subject));
    }
}
