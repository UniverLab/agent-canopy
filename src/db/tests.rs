use super::*;
use crate::domain::models::{Agent, Cli, RunLog, RunStatus, Trigger, TriggerType, WatchEvent};
use crate::domain::sync::{
    IntentPayload, MessageKind, MissionImpact, StatusPayload, WorkspaceStatus,
};
use chrono::{Duration, Utc};
use tempfile::{tempdir, NamedTempFile};

/// Create an in-memory-like DB backed by a temp file (`SQLite` needs a real file for WAL).
fn test_db() -> Database {
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    Database::new(&path).expect("create test db")
}

fn sample_cron_agent(id: &str) -> Agent {
    Agent {
        id: id.to_string(),
        prompt: "Run tests".to_string(),
        trigger: Some(Trigger::Cron {
            schedule_expr: "0 9 * * *".to_string(),
        }),
        cli: Cli::new("opencode"),
        model: None,
        working_dir: Some("/tmp/project".to_string()),
        enabled: true,
        created_at: Utc::now(),
        log_path: "/tmp/test.log".to_string(),
        timeout_minutes: 15,
        expires_at: None,
        last_run_at: None,
        last_run_ok: None,
        last_triggered_at: None,
        trigger_count: 0,
    }
}

fn sample_watch_agent(id: &str) -> Agent {
    Agent {
        id: id.to_string(),
        prompt: "Handle file change".to_string(),
        trigger: Some(Trigger::Watch {
            path: "/tmp/watched".to_string(),
            events: vec![WatchEvent::Create, WatchEvent::Modify],
            debounce_seconds: 5,
            recursive: true,
        }),
        cli: Cli::new("kiro"),
        model: Some("claude-4".to_string()),
        working_dir: None,
        enabled: true,
        created_at: Utc::now(),
        log_path: format!("/tmp/{}.log", id),
        timeout_minutes: 15,
        expires_at: None,
        last_run_at: None,
        last_run_ok: None,
        last_triggered_at: None,
        trigger_count: 0,
    }
}

fn sample_manual_agent(id: &str) -> Agent {
    Agent {
        id: id.to_string(),
        prompt: "Manual task".to_string(),
        trigger: None,
        cli: Cli::new("opencode"),
        model: None,
        working_dir: None,
        enabled: true,
        created_at: Utc::now(),
        log_path: "/tmp/manual.log".to_string(),
        timeout_minutes: 15,
        expires_at: None,
        last_run_at: None,
        last_run_ok: None,
        last_triggered_at: None,
        trigger_count: 0,
    }
}

// ── Terminal session lifecycle ──────────────────────────────────

#[test]
fn test_terminal_session_finish_removes_from_active_list() {
    let db = test_db();
    db.insert_terminal_session("term-1", "shell-1", "bash", "/tmp")
        .unwrap();

    let active = db.get_active_terminal_sessions().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, "term-1");

    db.finish_terminal_session("term-1").unwrap();
    assert!(db.get_active_terminal_sessions().unwrap().is_empty());
}

#[test]
fn test_mark_orphaned_terminal_sessions_clears_idle_records() {
    let db = test_db();
    db.insert_terminal_session("term-1", "shell-1", "bash", "/tmp")
        .unwrap();
    db.insert_terminal_session("term-2", "shell-2", "zsh", "/tmp")
        .unwrap();

    assert_eq!(db.get_active_terminal_sessions().unwrap().len(), 2);
    db.mark_orphaned_terminal_sessions().unwrap();
    assert!(db.get_active_terminal_sessions().unwrap().is_empty());
}

// ── Sync message lifecycle ───────────────────────────────────────

#[test]
fn test_list_sync_messages_returns_chronological_order() {
    let db = test_db();
    db.insert_sync_message(
        "/tmp/project",
        "agent-a",
        "copilot",
        MessageKind::Info,
        "one",
        None,
    )
    .unwrap();
    db.insert_sync_message(
        "/tmp/project",
        "agent-b",
        "claude",
        MessageKind::Query,
        "two",
        None,
    )
    .unwrap();

    let messages = db.list_sync_messages("/tmp/project", 10).unwrap();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].message, "one");
    assert_eq!(messages[1].message, "two");
}

#[test]
fn test_list_active_sync_agent_ids_includes_live_sessions_and_running_background_agents() {
    let db = test_db();
    db.insert_interactive_session("ix-1", "copilot", "copilot", "/tmp/project", None)
        .unwrap();
    db.insert_terminal_session("term-1", "shell", "bash", "/tmp/project")
        .unwrap();
    db.upsert_agent(&sample_cron_agent("bg-1")).unwrap();
    let run = RunLog {
        id: uuid::Uuid::new_v4().to_string(),
        background_agent_id: "bg-1".to_string(),
        status: RunStatus::InProgress,
        trigger_type: TriggerType::Scheduled,
        summary: None,
        started_at: Utc::now(),
        finished_at: None,
        exit_code: None,
        timeout_at: None,
    };
    db.insert_run(&run).unwrap();

    let mut ids = db.list_active_sync_agent_ids("/tmp/project").unwrap();
    ids.sort();

    assert_eq!(
        ids,
        vec!["bg-1".to_string(), "ix-1".to_string(), "term-1".to_string()]
    );
}

#[test]
fn test_sync_message_payload_roundtrip() {
    let db = test_db();
    let payload = serde_json::to_string(&IntentPayload {
        mission: "Refactor auth".to_string(),
        impact: MissionImpact::High,
        description: "touching login flow".to_string(),
    })
    .unwrap();
    db.insert_sync_message(
        "/tmp/project",
        "agent-a",
        "copilot",
        MessageKind::Intent,
        "copilot: Refactor auth",
        Some(&payload),
    )
    .unwrap();
    let status_payload = serde_json::to_string(&StatusPayload {
        status: WorkspaceStatus::Testing,
        message: "running smoke tests".to_string(),
    })
    .unwrap();
    db.insert_sync_message(
        "/tmp/project",
        "agent-a",
        "copilot",
        MessageKind::Status,
        "running smoke tests",
        Some(&status_payload),
    )
    .unwrap();

    let messages = db.list_sync_messages("/tmp/project", 10).unwrap();

    assert_eq!(messages[0].kind, MessageKind::Intent);
    assert_eq!(messages[1].kind, MessageKind::Status);
    assert!(messages[0]
        .payload
        .as_deref()
        .unwrap_or_default()
        .contains("Refactor auth"));
    assert!(messages[1]
        .payload
        .as_deref()
        .unwrap_or_default()
        .contains("testing"));
}

// ── Project registry / RAG metadata ──────────────────────────────

#[test]
fn test_register_project_path_extracts_readme_description() {
    let db = test_db();
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("README.md"),
        "# Title\n\nThis project description has enough words to satisfy the extractor and should become the default description for the registered project before any manual edits.\n",
    )
    .unwrap();

    let project = db.register_project_path(dir.path()).unwrap();

    assert_eq!(
        project.name,
        dir.path().file_name().unwrap().to_string_lossy()
    );
    assert!(project
        .description
        .as_deref()
        .unwrap_or_default()
        .contains("enough words"));
}

#[test]
fn test_upsert_project_preserves_existing_manual_description() {
    let db = test_db();
    let mut project = crate::domain::project::Project::new("/tmp/project");
    project.description = Some("Manual description".to_string());
    db.upsert_project(&project).unwrap();

    let mut updated = crate::domain::project::Project::new("/tmp/project");
    updated.description = Some("README description".to_string());
    db.upsert_project(&updated).unwrap();

    let stored = db.get_project(&project.hash).unwrap().unwrap();
    assert_eq!(stored.description.as_deref(), Some("Manual description"));
}

#[test]
fn test_mark_project_indexed_updates_timestamp() {
    let db = test_db();
    let project = crate::domain::project::Project::new("/tmp/project");
    db.upsert_project(&project).unwrap();

    let updated = db.mark_project_indexed(&project.hash, 1234).unwrap();
    assert!(updated);

    let stored = db.get_project(&project.hash).unwrap().unwrap();
    assert_eq!(stored.indexed_at, Some(1234));
}

#[test]
fn test_search_chunks_returns_recently_indexed_content() {
    let db = test_db();
    let project = crate::domain::project::Project::new("/tmp/project");
    db.upsert_project(&project).unwrap();
    db.replace_chunks(
        "src/lib.rs",
        &[crate::db::project::Chunk {
            id: "chunk-1".to_string(),
            project_hash: Some(project.hash.clone()),
            source_path: "src/lib.rs".to_string(),
            chunk_index: 0,
            content: "needle indexed chunk".to_string(),
            lang: "rust".to_string(),
            embedding: Some(vec![1.0, 0.0, 0.0]),
            updated_at: 1,
        }],
    )
    .unwrap();

    let results = db.search_chunks("needle", Some(&project.hash), 5).unwrap();

    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("needle indexed chunk"));
    assert_eq!(results[0].embedding.as_deref(), Some(&[1.0, 0.0, 0.0][..]));
}

#[test]
fn test_rag_queue_roundtrip() {
    let db = test_db();
    let project = crate::domain::project::Project::new("/tmp/project");
    db.upsert_project(&project).unwrap();

    db.enqueue_rag_item("/tmp/project/src/lib.rs", 111).unwrap();
    db.mark_rag_item_processing("/tmp/project/src/lib.rs", 222)
        .unwrap();

    let items = db.list_rag_queue(10).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, "processing");

    db.remove_rag_item("/tmp/project/src/lib.rs").unwrap();
    assert!(db.list_rag_queue(10).unwrap().is_empty());
}

#[test]
fn test_search_chunks_by_embedding_returns_best_match_first() {
    let db = test_db();
    db.replace_chunks(
        "src/lib.rs",
        &[
            crate::db::project::Chunk {
                id: "chunk-1".to_string(),
                project_hash: None,
                source_path: "src/lib.rs".to_string(),
                chunk_index: 0,
                content: "alpha".to_string(),
                lang: "text".to_string(),
                embedding: Some(vec![1.0, 0.0, 0.0]),
                updated_at: 1,
            },
            crate::db::project::Chunk {
                id: "chunk-2".to_string(),
                project_hash: None,
                source_path: "src/lib.rs".to_string(),
                chunk_index: 1,
                content: "beta".to_string(),
                lang: "text".to_string(),
                embedding: Some(vec![0.0, 1.0, 0.0]),
                updated_at: 2,
            },
        ],
    )
    .unwrap();

    let results = db
        .search_chunks_by_embedding(&[0.9, 0.1, 0.0], None, 2)
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].chunk.id, "chunk-1");
    assert!(results[0].score > results[1].score);
    assert_eq!(
        results[0].chunk.embedding.as_deref(),
        Some(&[1.0, 0.0, 0.0][..])
    );
}

#[test]
fn test_rag_info_summary_counts_chunks_and_queue_states() {
    let db = test_db();
    let project = crate::domain::project::Project::new("/tmp/project");
    db.upsert_project(&project).unwrap();

    db.enqueue_rag_item("/tmp/project/src/lib.rs", 111).unwrap();
    db.enqueue_rag_item("/tmp/project/src/main.rs", 112)
        .unwrap();
    db.mark_rag_item_processing("/tmp/project/src/main.rs", 113)
        .unwrap();

    db.replace_chunks(
        "/tmp/project/src/lib.rs",
        &[crate::db::project::Chunk {
            id: "chunk-1".to_string(),
            project_hash: Some(project.hash),
            source_path: "/tmp/project/src/lib.rs".to_string(),
            chunk_index: 0,
            content: "needle".to_string(),
            lang: "rust".to_string(),
            embedding: None,
            updated_at: 114,
        }],
    )
    .unwrap();

    let summary = db.rag_info_summary().unwrap();
    assert_eq!(summary.total_chunks, 1);
    assert_eq!(summary.indexed_projects, 1);
    assert_eq!(summary.queued_items, 1);
    assert_eq!(summary.processing_items, 1);
}

// ── Agent CRUD ─────────────────────────────────────────────────────

#[test]
fn test_upsert_and_get_cron_agent() {
    let db = test_db();
    let agent = sample_cron_agent("build-daily");
    db.upsert_agent(&agent).unwrap();

    let retrieved = db.get_agent("build-daily").unwrap().expect("agent exists");
    assert_eq!(retrieved.id, "build-daily");
    assert_eq!(retrieved.prompt, "Run tests");
    assert!(retrieved.is_cron());
    assert_eq!(retrieved.schedule_expr(), Some("0 9 * * *"));
    assert_eq!(retrieved.cli.as_str(), "opencode");
    assert_eq!(retrieved.working_dir.as_deref(), Some("/tmp/project"));
    assert!(retrieved.enabled);
}

#[test]
fn test_upsert_and_get_watch_agent() {
    let db = test_db();
    let agent = sample_watch_agent("watch-src");
    db.upsert_agent(&agent).unwrap();

    let retrieved = db.get_agent("watch-src").unwrap().expect("agent exists");
    assert_eq!(retrieved.id, "watch-src");
    assert!(retrieved.is_watch());
    assert_eq!(retrieved.watch_path(), Some("/tmp/watched"));
    let events = retrieved.watch_events().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.contains(&WatchEvent::Create));
    assert!(events.contains(&WatchEvent::Modify));
    assert_eq!(retrieved.cli.as_str(), "kiro");
    assert_eq!(retrieved.model.as_deref(), Some("claude-4"));
}

#[test]
fn test_get_nonexistent_agent() {
    let db = test_db();
    let result = db.get_agent("does-not-exist").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_upsert_agent_overwrites() {
    let db = test_db();
    let mut agent = sample_cron_agent("my-agent");
    db.upsert_agent(&agent).unwrap();

    agent.prompt = "Updated prompt".to_string();
    agent.trigger = Some(Trigger::Cron {
        schedule_expr: "*/10 * * * *".to_string(),
    });
    db.upsert_agent(&agent).unwrap();

    let retrieved = db.get_agent("my-agent").unwrap().unwrap();
    assert_eq!(retrieved.prompt, "Updated prompt");
    assert_eq!(retrieved.schedule_expr(), Some("*/10 * * * *"));
}

#[test]
fn test_list_agents_ordered_by_created_at_desc() {
    let db = test_db();

    let mut a1 = sample_cron_agent("first");
    a1.created_at = Utc::now() - Duration::hours(2);
    let mut a2 = sample_cron_agent("second");
    a2.created_at = Utc::now() - Duration::hours(1);
    let mut a3 = sample_cron_agent("third");
    a3.created_at = Utc::now();

    db.upsert_agent(&a1).unwrap();
    db.upsert_agent(&a2).unwrap();
    db.upsert_agent(&a3).unwrap();

    let agents = db.list_agents().unwrap();
    assert_eq!(agents.len(), 3);
    assert_eq!(agents[0].id, "third");
    assert_eq!(agents[1].id, "second");
    assert_eq!(agents[2].id, "first");
}

#[test]
fn test_list_cron_agents_filters_correctly() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("cron-1")).unwrap();
    db.upsert_agent(&sample_watch_agent("watch-1")).unwrap();
    db.upsert_agent(&sample_manual_agent("manual-1")).unwrap();

    let cron_agents = db.list_cron_agents().unwrap();
    assert_eq!(cron_agents.len(), 1);
    assert!(cron_agents[0].is_cron());
}

#[test]
fn test_list_watch_agents_filters_correctly() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("cron-1")).unwrap();
    db.upsert_agent(&sample_watch_agent("watch-1")).unwrap();
    db.upsert_agent(&sample_manual_agent("manual-1")).unwrap();

    let watch_agents = db.list_watch_agents().unwrap();
    assert_eq!(watch_agents.len(), 1);
    assert!(watch_agents[0].is_watch());
}

#[test]
fn test_delete_agent() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("to-delete")).unwrap();
    assert!(db.get_agent("to-delete").unwrap().is_some());

    db.delete_agent("to-delete").unwrap();
    assert!(db.get_agent("to-delete").unwrap().is_none());
}

#[test]
fn test_update_agent_enabled() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("toggle-me")).unwrap();

    db.update_agent_enabled("toggle-me", false).unwrap();
    let agent = db.get_agent("toggle-me").unwrap().unwrap();
    assert!(!agent.enabled);

    db.update_agent_enabled("toggle-me", true).unwrap();
    let agent = db.get_agent("toggle-me").unwrap().unwrap();
    assert!(agent.enabled);
}

#[test]
fn test_update_agent_last_run() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("run-me")).unwrap();

    db.update_agent_last_run("run-me", true).unwrap();
    let agent = db.get_agent("run-me").unwrap().unwrap();
    assert!(agent.last_run_at.is_some());
    assert_eq!(agent.last_run_ok, Some(true));

    db.update_agent_last_run("run-me", false).unwrap();
    let agent = db.get_agent("run-me").unwrap().unwrap();
    assert_eq!(agent.last_run_ok, Some(false));
}

#[test]
fn test_update_agent_triggered() {
    let db = test_db();
    db.upsert_agent(&sample_watch_agent("trig-w")).unwrap();

    db.update_agent_triggered("trig-w").unwrap();
    let agent = db.get_agent("trig-w").unwrap().unwrap();
    assert!(agent.last_triggered_at.is_some());
    assert_eq!(agent.trigger_count, 1);

    db.update_agent_triggered("trig-w").unwrap();
    let agent = db.get_agent("trig-w").unwrap().unwrap();
    assert_eq!(agent.trigger_count, 2);
}

#[test]
fn test_agent_with_expiration() {
    let db = test_db();
    let mut agent = sample_cron_agent("expiring");
    agent.expires_at = Some(Utc::now() + Duration::hours(1));
    db.upsert_agent(&agent).unwrap();

    let retrieved = db.get_agent("expiring").unwrap().unwrap();
    assert!(retrieved.expires_at.is_some());
    assert!(!retrieved.is_expired());
}

#[test]
fn test_manual_agent_roundtrip() {
    let db = test_db();
    let agent = sample_manual_agent("manual-task");
    db.upsert_agent(&agent).unwrap();

    let retrieved = db.get_agent("manual-task").unwrap().unwrap();
    assert_eq!(retrieved.id, "manual-task");
    assert!(retrieved.trigger.is_none());
    assert!(!retrieved.is_cron());
    assert!(!retrieved.is_watch());
    assert_eq!(retrieved.trigger_type_label(), "manual");
}

// ── Run log operations ────────────────────────────────────────────

#[test]
fn test_insert_and_list_runs() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("run-agent")).unwrap();

    let run = RunLog {
        id: uuid::Uuid::new_v4().to_string(),
        background_agent_id: "run-agent".to_string(),
        status: RunStatus::Success,
        trigger_type: TriggerType::Scheduled,
        summary: None,
        started_at: Utc::now() - Duration::minutes(5),
        finished_at: Some(Utc::now()),
        exit_code: Some(0),
        timeout_at: None,
    };
    db.insert_run(&run).unwrap();

    let runs = db.list_runs("run-agent", 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].background_agent_id, "run-agent");
    assert_eq!(runs[0].exit_code, Some(0));
    assert!(matches!(runs[0].trigger_type, TriggerType::Scheduled));
}

#[test]
fn test_list_runs_limit() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("many-runs")).unwrap();

    for i in 0..10 {
        let run = RunLog {
            id: uuid::Uuid::new_v4().to_string(),
            background_agent_id: "many-runs".to_string(),
            status: RunStatus::Success,
            trigger_type: TriggerType::Manual,
            summary: None,
            started_at: Utc::now() - Duration::minutes(i),
            finished_at: Some(Utc::now()),
            exit_code: Some(0),
            timeout_at: None,
        };
        db.insert_run(&run).unwrap();
    }

    let runs = db.list_runs("many-runs", 3).unwrap();
    assert_eq!(runs.len(), 3);
}

#[test]
fn test_delete_agent_cascades_runs() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("cascade-agent"))
        .unwrap();
    let run = RunLog {
        id: uuid::Uuid::new_v4().to_string(),
        background_agent_id: "cascade-agent".to_string(),
        status: RunStatus::Pending,
        trigger_type: TriggerType::Watch,
        summary: None,
        started_at: Utc::now(),
        finished_at: None,
        exit_code: None,
        timeout_at: None,
    };
    db.insert_run(&run).unwrap();
    assert_eq!(db.list_runs("cascade-agent", 10).unwrap().len(), 1);

    db.delete_agent("cascade-agent").unwrap();
    assert_eq!(db.list_runs("cascade-agent", 10).unwrap().len(), 0);
}

#[test]
fn test_update_run_status() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("status-agent")).unwrap();

    let run_id = uuid::Uuid::new_v4().to_string();
    let run = RunLog {
        id: run_id.clone(),
        background_agent_id: "status-agent".to_string(),
        status: RunStatus::Pending,
        trigger_type: TriggerType::Scheduled,
        summary: None,
        started_at: Utc::now(),
        finished_at: None,
        exit_code: None,
        timeout_at: None,
    };
    db.insert_run(&run).unwrap();

    let ok = db
        .update_run_status(&run_id, RunStatus::Success, Some("Done"))
        .unwrap();
    assert!(ok);

    let updated = db.get_run(&run_id).unwrap().unwrap();
    assert!(matches!(updated.status, RunStatus::Success));
    assert_eq!(updated.summary.as_deref(), Some("Done"));
    assert!(updated.finished_at.is_some());
}

#[test]
fn test_update_run_exit_code() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("exit-agent")).unwrap();

    let run_id = uuid::Uuid::new_v4().to_string();
    let run = RunLog {
        id: run_id.clone(),
        background_agent_id: "exit-agent".to_string(),
        status: RunStatus::Success,
        trigger_type: TriggerType::Manual,
        summary: Some("OK".to_string()),
        started_at: Utc::now(),
        finished_at: Some(Utc::now()),
        exit_code: None,
        timeout_at: None,
    };
    db.insert_run(&run).unwrap();

    let ok = db.update_run_exit_code(&run_id, 0).unwrap();
    assert!(ok);

    let updated = db.get_run(&run_id).unwrap().unwrap();
    assert_eq!(updated.exit_code, Some(0));
}

// ── Daemon state ──────────────────────────────────────────────

#[test]
fn test_set_and_get_state() {
    let db = test_db();
    db.set_state("port", "7755").unwrap();
    assert_eq!(db.get_state("port").unwrap(), Some("7755".to_string()));
}

#[test]
fn test_get_state_missing_key() {
    let db = test_db();
    assert!(db.get_state("missing").unwrap().is_none());
}

#[test]
fn test_set_state_overwrites() {
    let db = test_db();
    db.set_state("version", "0.1.0").unwrap();
    db.set_state("version", "0.2.0").unwrap();
    assert_eq!(db.get_state("version").unwrap(), Some("0.2.0".to_string()));
}
