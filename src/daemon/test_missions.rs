use crate::db::Database;
use crate::domain::sync::MessageKind;
use tempfile::tempdir;

#[test]
fn test_mission_persistence() {
    // Setup temporary database
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_missions.db");

    // Initialize schema (assuming this is required based on Database::new or similar initialization)
    // Actually, Database::new seems to open/create. Let's hope the schema is initialized.
    // If not, I might need to run migrations.
    // Looking at the codebase, Database::new seems to call `Database::init_schema`.
    let db = Database::new(&db_path).expect("Failed to create database");

    // Test data
    let workdir = "/tmp/test";
    let agent_id = "agent-1";
    let agent_name = "test-agent";
    let message = "Declaring mission: Test mission persistence";
    let payload = Some(r#"{"mission":"Test mission","impact":"low"}"#);

    // 1. Declare intent (persist sync message)
    let sync_msg = db
        .insert_sync_message(
            workdir,
            agent_id,
            agent_name,
            MessageKind::Info,
            message,
            payload,
        )
        .expect("Failed to insert sync message");

    assert_eq!(sync_msg.message, message);
    assert_eq!(sync_msg.payload, payload.map(str::to_owned));

    // 2. Retrieve and verify
    let messages = db
        .list_sync_messages(workdir, 10)
        .expect("Failed to list sync messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message, message);
    assert_eq!(messages[0].payload, payload.map(str::to_owned));
}
