//! Unit tests for daemon module

use crate::application::ports::StateRepository;
use crate::daemon::process::is_process_running;
use crate::db::Database;
use tempfile::tempdir;

#[test]
fn test_process_management() {
    // Test that process management functions work
    // Note: These are integration tests that may fail in CI

    // Test is_process_running with current process
    let current_pid = std::process::id();
    let result = is_process_running(current_pid);
    // is_process_running returns bool, not Result<bool>
    assert!(result);

    // Test with non-existent process (likely to be false)
    let _result = is_process_running(999999);
    // We can't assert !result because the process might exist
    // Just verify the function doesn't panic
}

#[test]
fn test_database_operations() {
    // Test daemon database operations
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::new(&db_path).unwrap();

    // Test setting and getting daemon state
    assert!(db.set_state("port", "7755").is_ok());
    assert!(db.set_state("version", "2.0.0").is_ok());
    assert!(db.set_state("last_start", "2024-01-01T00:00:00Z").is_ok());

    let port = db.get_state("port").unwrap();
    assert_eq!(port, Some("7755".to_string()));

    let version = db.get_state("version").unwrap();
    assert_eq!(version, Some("2.0.0".to_string()));
}
