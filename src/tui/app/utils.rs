use chrono::{DateTime, Utc};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// `(value, unit)` for a past instant, or `None` for anything under a
/// minute — the shared rounding rule behind both [`relative_time`] and
/// [`relative_time_compact`], so the two display forms never drift apart on
/// where an hour rounds up to a day, etc.
fn relative_time_parts(dt: &DateTime<Utc>) -> Option<(i64, char)> {
    let secs = Utc::now().signed_duration_since(*dt).num_seconds();
    if secs < 60 {
        None
    } else if secs < 3600 {
        Some((secs / 60, 'm'))
    } else if secs < 86400 {
        Some((secs / 3600, 'h'))
    } else {
        Some((secs / 86400, 'd'))
    }
}

pub fn relative_time(dt: &DateTime<Utc>) -> String {
    match relative_time_parts(dt) {
        None => "just now".to_string(),
        Some((n, unit)) => format!("{n}{unit} ago"),
    }
}

/// Compact form for narrow columns (sidebar loop rows): `2m`, `1h`, `3d`,
/// no "ago" suffix. Same thresholds as [`relative_time`] — see
/// [`relative_time_parts`].
pub fn relative_time_compact(dt: &DateTime<Utc>) -> String {
    match relative_time_parts(dt) {
        None => "now".to_string(),
        Some((n, unit)) => format!("{n}{unit}"),
    }
}

/// Compact form for a future instant relative to now: `2m`, `1h`, `3d`, or
/// `"due"` once `dt` has passed — used for a loop's pending `autorun_at` in
/// the sidebar. Mirrors [`relative_time_compact`]'s thresholds but counts
/// down instead of up.
pub fn relative_time_until_compact(dt: &DateTime<Utc>) -> String {
    let secs = dt.signed_duration_since(Utc::now()).num_seconds();
    if secs < 60 {
        "due".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

pub fn tail_lines(content: &str, n: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

pub fn is_process_running(pid: u32) -> bool {
    crate::daemon::process::is_process_running(pid)
}

pub fn is_local_port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    #[test]
    fn relative_time_compact_under_a_minute() {
        let dt = Utc::now() - ChronoDuration::seconds(30);
        assert_eq!(relative_time_compact(&dt), "now");
    }

    #[test]
    fn relative_time_compact_minutes() {
        let dt = Utc::now() - ChronoDuration::minutes(2);
        assert_eq!(relative_time_compact(&dt), "2m");
    }

    #[test]
    fn relative_time_compact_hours() {
        let dt = Utc::now() - ChronoDuration::hours(1);
        assert_eq!(relative_time_compact(&dt), "1h");
    }

    #[test]
    fn relative_time_compact_days() {
        let dt = Utc::now() - ChronoDuration::days(3);
        assert_eq!(relative_time_compact(&dt), "3d");
    }

    #[test]
    fn relative_time_compact_has_no_ago_suffix() {
        let dt = Utc::now() - ChronoDuration::hours(5);
        assert!(!relative_time_compact(&dt).contains("ago"));
    }

    #[test]
    fn relative_time_and_compact_share_the_same_rounding_boundary() {
        // Both forms must flip from minutes to hours at the same instant —
        // otherwise the sidebar and other panels would disagree about how
        // long ago something happened.
        let dt = Utc::now() - ChronoDuration::seconds(3600);
        assert!(relative_time(&dt).starts_with("1h"));
        assert!(relative_time_compact(&dt).starts_with("1h"));
    }
}
