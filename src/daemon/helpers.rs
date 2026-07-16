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
