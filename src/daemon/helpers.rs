use std::sync::Arc;

use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;

use crate::application::notification_service::NotificationService;

pub(crate) fn data_dir() -> Result<std::path::PathBuf, McpError> {
    let home = dirs::home_dir()
        .ok_or_else(|| McpError::internal_error("Home directory not found", None))?;
    Ok(home.join(".canopy"))
}

pub(crate) fn success_result(message: &str) -> CallToolResult {
    CallToolResult::success(vec![rmcp::model::Content::text(message.to_string())])
}

pub(crate) fn error_result(message: &str) -> CallToolResult {
    CallToolResult::error(vec![rmcp::model::Content::text(message.to_string())])
}

pub(crate) fn filter_log_line(
    line: &str,
    since_dt: &chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    if !line.starts_with("--- [") {
        return true;
    }
    let Some(at_pos) = line.find(" at ") else {
        return true;
    };
    let rest = &line[at_pos + 4..];
    let Some(end) = rest.find(" ---") else {
        return true;
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&rest[..end]) else {
        return true;
    };
    dt >= *since_dt
}

pub(crate) fn notify_run_result(
    notification_service: &Arc<dyn NotificationService>,
    id: &str,
    result: Result<i32, anyhow::Error>,
) {
    match result {
        Ok(code) => {
            // Runs that actually started are notified by the executor's own
            // notify_result — toasting here again double-notified every
            // manual run.
            tracing::info!("Manual run '{}' finished (exit {})", id, code);
        }
        Err(e) => {
            // The run never started, so the executor never got the chance to
            // notify — this is the only place that can surface the failure.
            tracing::error!("Manual run '{}' failed: {}", id, e);
            notification_service.notify_task_failed(id, 1, &e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// success_result and error_result don't panic on various inputs
    #[test]
    fn success_result_no_panic() {
        let _ = success_result("Test success");
        let _ = success_result("");
        let _ = success_result("Multi\nline\nmessage");
    }

    /// error_result doesn't panic on various inputs
    #[test]
    fn error_result_no_panic() {
        let _ = error_result("Test error");
        let _ = error_result("");
        let _ = error_result("Error with special chars: <>&\"'");
    }

    /// filter_log_line returns true for non-prefixed lines
    #[test]
    fn filter_log_line_passes_normal_lines() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap();
        assert!(filter_log_line("normal log line", &since));
        assert!(filter_log_line("another line without prefix", &since));
        assert!(filter_log_line("", &since));
    }

    /// filter_log_line returns true if timestamp bracket format is wrong
    #[test]
    fn filter_log_line_passes_malformed_bracketed_lines() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap();
        assert!(filter_log_line("--- [something] no timestamp", &since));
        assert!(filter_log_line("--- [without closing", &since));
    }

    /// filter_log_line returns true if timestamp is missing or malformed
    #[test]
    fn filter_log_line_passes_malformed_timestamp() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap();
        assert!(filter_log_line(
            "--- [something] at not-a-timestamp ---",
            &since
        ));
        assert!(filter_log_line(
            "--- [something] at 2024-01-01 (wrong format)",
            &since
        ));
    }

    /// filter_log_line returns true if " --- " suffix is missing
    #[test]
    fn filter_log_line_passes_lines_missing_closing_marker() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap();
        assert!(filter_log_line(
            "--- [x] at 2024-01-01T12:00:00+00:00",
            &since
        ));
        assert!(filter_log_line(
            "--- [x] at 2024-01-01T12:00:00+00:00 (no closing)",
            &since
        ));
    }

    /// filter_log_line filters out timestamps before the since time
    #[test]
    fn filter_log_line_filters_old_timestamps() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-02T00:00:00+00:00").unwrap();
        let old_log = "--- [event] at 2024-01-01T23:59:59+00:00 ---";
        assert!(!filter_log_line(old_log, &since));
    }

    /// filter_log_line passes timestamps at or after since time
    #[test]
    fn filter_log_line_passes_recent_timestamps() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-02T00:00:00+00:00").unwrap();
        let recent_log = "--- [event] at 2024-01-02T00:00:00+00:00 ---";
        assert!(filter_log_line(recent_log, &since));

        let future_log = "--- [event] at 2024-01-02T01:00:00+00:00 ---";
        assert!(filter_log_line(future_log, &since));
    }

    /// filter_log_line handles microseconds and timezones correctly
    #[test]
    fn filter_log_line_handles_rfc3339_timestamps() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:30:45+00:00").unwrap();

        // Just at boundary
        let boundary = "--- [x] at 2024-01-01T12:30:45+00:00 ---";
        assert!(filter_log_line(boundary, &since));

        // Before boundary (should be filtered)
        let before = "--- [x] at 2024-01-01T12:30:44+00:00 ---";
        assert!(!filter_log_line(before, &since));

        // After boundary
        let after = "--- [x] at 2024-01-01T12:30:46+00:00 ---";
        assert!(filter_log_line(after, &since));
    }

    /// filter_log_line works with different timezone offsets
    #[test]
    fn filter_log_line_respects_timezones() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00+00:00").unwrap();

        // UTC+2, 10:00 is earlier than UTC 12:00
        let eastern = "--- [x] at 2024-01-01T10:00:00+02:00 ---";
        assert!(!filter_log_line(eastern, &since));

        // UTC-5, 17:00 is later than UTC 12:00
        let us_eastern = "--- [x] at 2024-01-01T17:00:00-05:00 ---";
        assert!(filter_log_line(us_eastern, &since));
    }

    /// data_dir would return home/.canopy if home exists (integration test)
    #[test]
    fn data_dir_path_structure() {
        // This test verifies the function signature works
        // In actual testing environment, if home_dir() is available, it should return .canopy
        if dirs::home_dir().is_some() {
            let result = data_dir();
            assert!(result.is_ok());
            if let Ok(path) = result {
                assert!(path.ends_with(".canopy"));
            }
        }
    }

    #[test]
    fn success_result_creates_success_call_tool_result() {
        let result = success_result("Operation completed");
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn error_result_creates_error_call_tool_result() {
        let result = error_result("Something went wrong");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn filter_log_line_handles_malformed_timestamp() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap();
        let malformed = "--- [event] at not-a-timestamp ---";
        assert!(filter_log_line(malformed, &since));
    }

    #[test]
    fn filter_log_line_handles_missing_at_marker() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap();
        let no_at = "--- [event] without at marker ---";
        assert!(filter_log_line(no_at, &since));
    }

    #[test]
    fn filter_log_line_handles_missing_end_marker() {
        let since = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap();
        let no_end = "--- [event] at 2024-01-01T12:00:00+00:00";
        assert!(filter_log_line(no_end, &since));
    }
}
