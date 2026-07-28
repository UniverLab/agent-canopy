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

#[test]
fn test_substitute_variables_all_vars() {
    let prompt = "Task {{TASK_ID}} at {{TIMESTAMP}} on {{FILE_PATH}} for {{EVENT_TYPE}}";
    let result = substitute_variables(
        prompt,
        "task-123",
        "/var/log/task.log",
        Some("/src/main.rs"),
        Some("modify"),
    );
    assert!(result.contains("task-123"));
    assert!(result.contains("/src/main.rs"));
    assert!(result.contains("modify"));
    assert!(result.contains("/var/log/task.log"));
}

#[test]
fn test_substitute_variables_no_file_or_event() {
    let prompt = "Run task {{TASK_ID}}";
    let result = substitute_variables(prompt, "task-1", "/tmp/log.log", None, None);
    assert!(result.contains("task-1"));
    assert!(!result.contains("{{FILE_PATH}}"));
    assert!(!result.contains("{{EVENT_TYPE}}"));
}

#[test]
fn test_substitute_variables_no_template_vars() {
    let prompt = "Simple prompt without variables";
    let result = substitute_variables(prompt, "task-x", "/tmp/log.log", None, None);
    assert_eq!(result, prompt);
}

#[test]
fn test_validate_cron_edge_cases() {
    assert!(validate_cron("* * * * *"));
    assert!(validate_cron("0 0 * * *"));
    assert!(validate_cron("59 23 * * *"));
    assert!(validate_cron("*/15 * * * *"));
    assert!(validate_cron("0,30 * * * *"));
    assert!(validate_cron("0-5 * * * *"));
}
