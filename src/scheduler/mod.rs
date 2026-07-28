//! Schedule parsing, variable substitution, and internal cron scheduling.
//!
//! Provides:
//! - Cron expression validation
//! - Prompt template variable substitution
//! - Internal cron scheduler (tokio-based, replaces OS crontab/launchd)

pub mod cron_scheduler;

use chrono::Utc;

/// Substitute template variables in a background_agent prompt.
///
/// Supported variables:
/// - `{{TIMESTAMP}}` — current ISO 8601 timestamp
/// - `{{TASK_ID}}` — the background_agent's ID
/// - `{{LOG_PATH}}` — the background_agent's log file path
/// - `{{FILE_PATH}}` — the watched file path (watchers only)
/// - `{{EVENT_TYPE}}` — the event type (watchers only)
pub fn substitute_variables(
    prompt: &str,
    background_agent_id: &str,
    log_path: &str,
    file_path: Option<&str>,
    event_type: Option<&str>,
) -> String {
    let timestamp = Utc::now().to_rfc3339();
    let mut result = prompt.to_string();

    result = result.replace("{{TIMESTAMP}}", &timestamp);
    result = result.replace("{{TASK_ID}}", background_agent_id);
    result = result.replace("{{LOG_PATH}}", log_path);

    if let Some(path) = file_path {
        result = result.replace("{{FILE_PATH}}", path);
    }
    if let Some(event) = event_type {
        result = result.replace("{{EVENT_TYPE}}", event);
    }

    result
}

/// Validate that a string is a valid 5-field cron expression.
///
/// The model is responsible for converting natural language to cron.
/// This function only validates the format.
pub fn validate_cron(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    for part in parts {
        if part.is_empty() {
            return false;
        }
        // Allow: digits, /, -, comma, *
        if part != "*"
            && !part
                .chars()
                .all(|c| c.is_ascii_digit() || c == '/' || c == '-' || c == ',' || c == '*')
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_variables() {
        let prompt = "BackgroundAgent {{TASK_ID}} at {{TIMESTAMP}} on {{FILE_PATH}}";
        let result = substitute_variables(
            prompt,
            "my-background_agent",
            "/logs/my-background_agent.log",
            Some("/home/file.txt"),
            None,
        );
        assert!(result.contains("my-background_agent"));
        assert!(result.contains("/home/file.txt"));
        assert!(!result.contains("{{TIMESTAMP}}"));
    }

    #[test]
    fn test_validate_cron_valid() {
        assert!(validate_cron("*/5 * * * *"));
        assert!(validate_cron("0 9 * * *"));
        assert!(validate_cron("0 9 * * 1-5"));
        assert!(validate_cron("30 14 1,15 * *"));
        assert!(validate_cron("0 */2 * * *"));
    }

    #[test]
    fn test_validate_cron_invalid() {
        assert!(!validate_cron("every 5 minutes"));
        assert!(!validate_cron("daily at 9am"));
        assert!(!validate_cron("* * *")); // only 3 fields
        assert!(!validate_cron("")); // empty
        assert!(!validate_cron("0 9 * * * *")); // 6 fields
    }
}
