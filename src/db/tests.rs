use super::*;
use crate::db::intelligence::{IntelligenceNodeInput, IntelligenceRelationInput};
use crate::domain::loops::{
    Loop, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus,
    LoopSpec, LoopSpecStatus, LoopStatus, SpecAdminStatusOutcome,
};
use crate::domain::models::{Agent, Cli, RunLog, RunStatus, Trigger, TriggerType, WatchEvent};
use crate::domain::pools::Pool;
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
        enable_at: None,
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
        enable_at: None,
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
        enable_at: None,
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

fn sample_loop(id: &str) -> Loop {
    Loop {
        id: id.to_string(),
        name: "Auth loop".to_string(),
        description: Some("Implements auth in ordered specs".to_string()),
        workdir: "/tmp/project".to_string(),
        status: LoopStatus::Draft,
        trigger: None,
        created_at: Utc::now(),
        started_at: None,
        completed_at: None,
        autorun_at: None,
        active_run_pool_id: None,
        on_completed: None,
    }
}

fn sample_loop_spec(loop_id: &str, id: &str, position: i64) -> LoopSpec {
    LoopSpec {
        id: id.to_string(),
        loop_id: Some(loop_id.to_string()),
        name: format!("Spec {position}"),
        description: Some("Do a slice of the feature".to_string()),
        position,
        parallelizable: false,
        status: LoopSpecStatus::Pending,
        started_at: None,
        completed_at: None,
        spec_start_head: None,
        workdir: None,
        completed_via: None,
        completed_via_reason: None,
        completed_via_at: None,
    }
}

fn sample_loop_node(spec_id: &str, id: &str, position: i64) -> LoopNode {
    LoopNode {
        id: id.to_string(),
        spec_id: Some(spec_id.to_string()),
        loop_id: None,
        name: format!("Node {position}"),
        kind: LoopNodeKind::Check,
        config: serde_json::json!({
            "command": "cargo test",
            "success_condition": "exit_code_0"
        }),
        position,
        created_at: Utc::now(),
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

// ── B32: unrecoverable interactive sessions close instead of orphaning ──

#[test]
fn mark_session_closed_retires_active_session_to_completed() {
    let db = test_db();
    db.insert_interactive_session(
        "sess-dead",
        "s",
        "claude",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    assert_eq!(db.get_active_sessions().unwrap().len(), 1);

    db.mark_session_closed("sess-dead").unwrap();

    // Gone from the active sidebar list, never surfaces as a red orphan, and
    // the row is kept for history in the terminal `completed` status.
    assert!(db.get_active_sessions().unwrap().is_empty());
    assert!(db.get_orphaned_sessions().unwrap().is_empty());
    assert_eq!(
        db.get_interactive_session_status("sess-dead").unwrap(),
        Some("completed".to_string())
    );
}

#[test]
fn mark_session_closed_is_noop_for_non_active_session() {
    let db = test_db();
    db.insert_interactive_session(
        "sess-resumed",
        "s",
        "claude",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    db.mark_session_resumed("sess-resumed").unwrap();

    // Guarded to `active` rows: a session already resumed elsewhere is left
    // untouched rather than being clobbered to `completed`.
    db.mark_session_closed("sess-resumed").unwrap();
    assert_eq!(
        db.get_interactive_session_status("sess-resumed").unwrap(),
        Some("resumed".to_string())
    );
}

#[test]
fn close_orphaned_interactive_sessions_sweeps_historic_orphans() {
    let db = test_db();
    // A historic orphan (written before orphaning was removed) plus a healthy
    // active session that must be left alone.
    db.insert_interactive_session(
        "sess-old",
        "old",
        "claude",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    db.mark_session_orphaned("sess-old").unwrap();
    db.insert_interactive_session(
        "sess-live",
        "live",
        "claude",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    assert_eq!(db.get_orphaned_sessions().unwrap().len(), 1);

    let swept = db.close_orphaned_interactive_sessions().unwrap();
    assert_eq!(swept, 1);

    // The orphan row disappears from the orphaned list (now `completed`, kept
    // for history); the active session is untouched.
    assert!(db.get_orphaned_sessions().unwrap().is_empty());
    assert_eq!(
        db.get_interactive_session_status("sess-old").unwrap(),
        Some("completed".to_string())
    );
    assert_eq!(
        db.get_interactive_session_status("sess-live").unwrap(),
        Some("active".to_string())
    );
}

#[test]
fn scheduled_sends_for_closed_session_are_dropped_on_missing_target_restore() {
    let db = test_db();
    db.insert_interactive_session(
        "sess-x",
        "s",
        "claude",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    db.insert_scheduled_send("ss-x", "ping", "sess-x", None, Utc::now())
        .unwrap();
    assert_eq!(
        db.list_pending_scheduled_sends_for_session("sess-x")
            .unwrap()
            .len(),
        1
    );

    // The session is closed as unrecoverable, so it is not resumed and never
    // joins the live-agent set. `restore_scheduled_sends`' missing-target drop
    // (modelled here with an empty live list) then discards its schedules —
    // the same path a session with a missing target already takes.
    db.mark_session_closed("sess-x").unwrap();
    let dropped = db.drop_scheduled_sends_missing_targets(&[]).unwrap();
    assert_eq!(dropped, 1);
    assert!(db
        .list_pending_scheduled_sends_for_session("sess-x")
        .unwrap()
        .is_empty());
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
    db.insert_interactive_session(
        "ix-1",
        "copilot",
        "copilot",
        "/tmp/project",
        None,
        None,
        "interactive",
        None,
    )
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

#[test]
fn test_recent_sync_messages_returns_global_order() {
    let db = test_db();
    db.insert_sync_message(
        "/tmp/project-a",
        "agent-a",
        "copilot",
        MessageKind::Info,
        "one",
        None,
    )
    .unwrap();
    db.insert_sync_message(
        "/tmp/project-b",
        "agent-b",
        "claude",
        MessageKind::Info,
        "two",
        None,
    )
    .unwrap();

    let messages = db.list_recent_sync_messages(10).unwrap();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].message, "one");
    assert_eq!(messages[1].message, "two");
}

#[test]
fn test_resolve_sync_actor_name_prefers_interactive_session_name() {
    let db = test_db();
    db.insert_interactive_session(
        "ix-1",
        "violet-river",
        "copilot",
        "/tmp/project",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    db.insert_sync_message(
        "/tmp/project",
        "ix-1",
        "Copilot CLI",
        MessageKind::Info,
        "hello",
        None,
    )
    .unwrap();

    let resolved = db.resolve_sync_actor_name("/tmp/project", "ix-1").unwrap();

    assert_eq!(resolved.as_deref(), Some("violet-river · copilot"));
}

#[test]
fn test_resolve_sync_actor_name_prefers_terminal_session_name() {
    let db = test_db();
    db.insert_terminal_session("term-1", "shell-sage", "bash", "/tmp/project")
        .unwrap();

    let resolved = db
        .resolve_sync_actor_name("/tmp/project", "term-1")
        .unwrap();

    assert_eq!(resolved.as_deref(), Some("shell-sage"));
}

#[test]
fn test_resolve_sync_actor_display_name_falls_back_to_agent_id() {
    let db = test_db();

    let resolved = db
        .resolve_sync_actor_display_name("/tmp/project", "bg-1")
        .unwrap();

    assert_eq!(resolved, "bg-1");
}

// ── Project context layer ─────────────────────────────────────────

#[test]
fn test_intelligence_upsert_search_and_graph_walk() {
    let db = test_db();
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("node-b".to_string()),
        kind: "pattern".to_string(),
        title: "Connection caching".to_string(),
        body: "Cache expensive clients".to_string(),
        metadata: None,
        project_hash: Some("project-1".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();
    let base = db
        .upsert_intelligence_node(IntelligenceNodeInput {
            id: Some("node-a".to_string()),
            kind: "fact".to_string(),
            title: "Database rule".to_string(),
            body: "Use a single connection".to_string(),
            metadata: Some(serde_json::json!({"topic": "db"})),
            project_hash: Some("project-1".to_string()),
            session_id: Some("session-1".to_string()),
            relations: Some(vec![IntelligenceRelationInput {
                to_node_id: "node-b".to_string(),
                relation: "related_to".to_string(),
                weight: Some(0.8),
            }]),
        })
        .unwrap();

    let search = db
        .search_intelligence_nodes("connection", Some("pattern"), 10)
        .unwrap();
    assert_eq!(search.len(), 1);
    assert_eq!(search[0].id, "node-b");

    let walk = db
        .walk_intelligence_graph(&base.id, 2)
        .unwrap()
        .expect("graph walk should find node");
    assert_eq!(walk.root.id, "node-a");
    assert!(walk.nodes.iter().any(|node| node.id == "node-b"));
    assert!(walk.edges.iter().any(|edge| edge.from_node_id == "node-a"));
}

#[test]
fn test_intelligence_search_tokenizes_multi_term_queries() {
    let db = test_db();
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("node-multi".to_string()),
        kind: "fact".to_string(),
        title: "Alpha overview".to_string(),
        body: "This section covers alpha in detail. Later on we discuss beta too.".to_string(),
        metadata: None,
        project_hash: Some("project-1".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    // 1. Terms in different places of the body both match with AND semantics.
    let both = db
        .search_intelligence_nodes("alpha beta", None, 10)
        .unwrap();
    assert_eq!(both.len(), 1);
    assert_eq!(both[0].id, "node-multi");

    // 2. A query with one absent term should not match.
    let missing = db.search_intelligence_nodes("alpha zzz", None, 10).unwrap();
    assert!(missing.is_empty());

    // 3. Single-term queries keep working as before.
    let single = db.search_intelligence_nodes("beta", None, 10).unwrap();
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].id, "node-multi");

    // 4. Empty/whitespace-only queries return an empty list.
    let empty = db.search_intelligence_nodes("   ", None, 10).unwrap();
    assert!(empty.is_empty());
}

// ── Loop persistence ──────────────────────────────────────────

#[test]
fn loop_details_roundtrip_preserves_order_and_graph() {
    let db = test_db();
    let lp = sample_loop("wf-1");
    let spec_one = sample_loop_spec(&lp.id, "spec-1", 1);
    let spec_two = sample_loop_spec(&lp.id, "spec-2", 2);
    let node_one = sample_loop_node(&spec_one.id, "node-1", 1);
    let node_two = sample_loop_node(&spec_one.id, "node-2", 2);
    let edge = LoopEdge {
        id: "edge-1".to_string(),
        spec_id: Some(spec_one.id.clone()),
        loop_id: None,
        from_node: node_one.id.clone(),
        to_node: node_two.id.clone(),
        condition: LoopEdgeCondition::Pass,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec_two).unwrap();
    db.insert_loop_spec(&spec_one).unwrap();
    db.insert_loop_node(&node_two).unwrap();
    db.insert_loop_node(&node_one).unwrap();
    db.insert_loop_edge(&edge).unwrap();

    let details = db.get_loop_details(&lp.id).unwrap().unwrap();

    assert_eq!(details.lp.id, lp.id);
    assert_eq!(details.specs.len(), 2);
    assert_eq!(details.specs[0].spec.id, spec_one.id);
    assert_eq!(details.specs[0].nodes[0].id, node_one.id);
    assert_eq!(details.specs[0].nodes[1].id, node_two.id);
    assert_eq!(details.specs[0].edges[0].id, edge.id);
    assert_eq!(details.specs[1].spec.id, spec_two.id);
    // Spec-level graph must be untouched by the loop-level graph work: no
    // loop-level nodes/edges were defined for this loop.
    assert!(details.graph_nodes.is_empty());
    assert!(details.graph_edges.is_empty());
}

#[test]
fn loop_level_graph_round_trips_through_insert_and_get_loop_details() {
    // R1: a loop can define its graph once, at the loop level, instead of
    // repeating the same nodes/edges in every spec. `get_loop_details` is
    // exactly what the `loop_get` MCP tool returns.
    let db = test_db();
    let lp = sample_loop("wf-graph");
    let node_one = LoopNode {
        id: "graph-node-1".to_string(),
        spec_id: None,
        loop_id: Some(lp.id.clone()),
        name: "implement".to_string(),
        kind: LoopNodeKind::Agent,
        config: serde_json::json!({"platform": "claude"}),
        position: 1,
        created_at: Utc::now(),
    };
    let node_two = LoopNode {
        id: "graph-node-2".to_string(),
        spec_id: None,
        loop_id: Some(lp.id.clone()),
        name: "review".to_string(),
        kind: LoopNodeKind::Gate,
        config: serde_json::json!({}),
        position: 2,
        created_at: Utc::now(),
    };
    let edge = LoopEdge {
        id: "graph-edge-1".to_string(),
        spec_id: None,
        loop_id: Some(lp.id.clone()),
        from_node: node_one.id.clone(),
        to_node: node_two.id.clone(),
        condition: LoopEdgeCondition::Always,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_node(&node_one).unwrap();
    db.insert_loop_node(&node_two).unwrap();
    db.insert_loop_edge(&edge).unwrap();

    let details = db.get_loop_details(&lp.id).unwrap().unwrap();

    assert_eq!(details.graph_nodes.len(), 2);
    assert_eq!(details.graph_nodes[0].id, node_one.id);
    assert_eq!(
        details.graph_nodes[0].loop_id.as_deref(),
        Some(lp.id.as_str())
    );
    assert_eq!(details.graph_nodes[0].spec_id, None);
    assert_eq!(details.graph_edges.len(), 1);
    assert_eq!(details.graph_edges[0].id, edge.id);
    assert_eq!(
        details.graph_edges[0].loop_id.as_deref(),
        Some(lp.id.as_str())
    );
    // A loop with no specs at all still round-trips a graph-only loop.
    assert!(details.specs.is_empty());
}

#[test]
fn loop_node_and_edge_require_exactly_one_target() {
    // R1: every node/edge must target exactly one of (spec_id, loop_id).
    // Enforced in the DB layer with an actionable error, not a raw SQLite
    // CHECK constraint failure.
    let db = test_db();
    let lp = sample_loop("wf-target-validation");
    let spec = sample_loop_spec(&lp.id, "spec-target-validation", 1);
    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();

    let base_node = LoopNode {
        id: "node-both-or-neither".to_string(),
        spec_id: None,
        loop_id: None,
        name: "n".to_string(),
        kind: LoopNodeKind::Check,
        config: serde_json::json!({"command": "true"}),
        position: 1,
        created_at: Utc::now(),
    };

    let neither_err = db.insert_loop_node(&base_node).unwrap_err().to_string();
    assert!(
        neither_err.contains("exactly one"),
        "expected actionable message, got: {neither_err}"
    );

    let mut both_node = base_node;
    both_node.spec_id = Some(spec.id.clone());
    both_node.loop_id = Some(lp.id.clone());
    let both_err = db.insert_loop_node(&both_node).unwrap_err().to_string();
    assert!(
        both_err.contains("exactly one"),
        "expected actionable message, got: {both_err}"
    );

    let base_edge = LoopEdge {
        id: "edge-both-or-neither".to_string(),
        spec_id: None,
        loop_id: None,
        from_node: "a".to_string(),
        to_node: "b".to_string(),
        condition: LoopEdgeCondition::Always,
    };
    let neither_edge_err = db.insert_loop_edge(&base_edge).unwrap_err().to_string();
    assert!(neither_edge_err.contains("exactly one"));

    let mut both_edge = base_edge;
    both_edge.spec_id = Some(spec.id);
    both_edge.loop_id = Some(lp.id);
    let both_edge_err = db.insert_loop_edge(&both_edge).unwrap_err().to_string();
    assert!(both_edge_err.contains("exactly one"));
}

#[test]
fn loop_graph_migration_adds_loop_id_and_is_idempotent_across_reopen() {
    // Simulate a pre-R1 database: `loop_nodes`/`loop_edges` with `spec_id
    // NOT NULL` and no `loop_id` column — the real shape of databases in the
    // field before this migration. The migration must rebuild both tables
    // (SQLite can't relax NOT NULL via ALTER TABLE ADD COLUMN) without
    // losing the existing rows, and running it again on an already-migrated
    // database must be a no-op.
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    {
        let conn = rusqlite::Connection::open(&path).expect("open raw legacy db");
        conn.execute_batch(
            "CREATE TABLE loops (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                workdir TEXT NOT NULL,
                status TEXT NOT NULL,
                trigger_type TEXT,
                trigger_config TEXT,
                created_at INTEGER NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                autorun_at INTEGER
             );
             CREATE TABLE loop_specs (
                id TEXT PRIMARY KEY,
                loop_id TEXT NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                description TEXT,
                position INTEGER NOT NULL,
                parallelizable INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                started_at INTEGER,
                completed_at INTEGER
             );
             CREATE TABLE loop_nodes (
                id TEXT PRIMARY KEY,
                spec_id TEXT NOT NULL REFERENCES loop_specs(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                config TEXT NOT NULL,
                position INTEGER NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX idx_loop_nodes_position ON loop_nodes(spec_id, position);
             CREATE TABLE loop_edges (
                id TEXT PRIMARY KEY,
                spec_id TEXT NOT NULL REFERENCES loop_specs(id) ON DELETE CASCADE,
                from_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                to_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                condition TEXT NOT NULL
             );
             INSERT INTO loops (id, name, workdir, status, created_at)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'draft', 0);
             INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES ('legacy-spec', 'legacy-loop', 'Spec', 1, 'pending');
             INSERT INTO loop_nodes (id, spec_id, name, kind, config, position, created_at)
                 VALUES ('legacy-node', 'legacy-spec', 'Node', 'check', '{\"command\":\"true\"}', 1, 0);
             INSERT INTO loop_edges (id, spec_id, from_node, to_node, condition)
                 VALUES ('legacy-edge', 'legacy-spec', 'legacy-node', 'legacy-node', 'always');",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must rebuild both
    // tables without losing the pre-existing rows.
    let db = Database::new(&path).expect("open db, running migration");
    let node = db.get_loop_node("legacy-node").unwrap().unwrap();
    assert_eq!(node.spec_id.as_deref(), Some("legacy-spec"));
    assert_eq!(node.loop_id, None);
    let edge = db.get_loop_edge("legacy-edge").unwrap().unwrap();
    assert_eq!(edge.spec_id.as_deref(), Some("legacy-spec"));
    assert_eq!(edge.loop_id, None);
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent).
    let db = Database::new(&path).expect("reopen db after migration already applied");
    let node = db.get_loop_node("legacy-node").unwrap().unwrap();
    assert_eq!(node.spec_id.as_deref(), Some("legacy-spec"));
    assert_eq!(
        db.list_loop_edges("legacy-spec").unwrap().len(),
        1,
        "spec-level edge must survive the rebuild"
    );

    // The rebuilt table now supports loop-level nodes/edges too.
    db.insert_loop_node(&LoopNode {
        id: "graph-node".to_string(),
        spec_id: None,
        loop_id: Some("legacy-loop".to_string()),
        name: "Graph node".to_string(),
        kind: LoopNodeKind::Agent,
        config: serde_json::json!({}),
        position: 1,
        created_at: Utc::now(),
    })
    .unwrap();
    assert_eq!(db.list_loop_nodes_for_loop("legacy-loop").unwrap().len(), 1);
}

#[test]
fn loop_specs_migration_relaxes_loop_id_and_adds_workdir_and_is_idempotent() {
    // Simulate a pre-R3 database: `loop_specs` with `loop_id NOT NULL` and
    // no `workdir` column — the real shape of databases in the field before
    // this migration. The migration must rebuild the table (SQLite can't
    // relax NOT NULL via ALTER TABLE ADD COLUMN) without losing existing
    // (loop-bound) rows, and running it again on an already-migrated
    // database must be a no-op.
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    {
        let conn = rusqlite::Connection::open(&path).expect("open raw legacy db");
        conn.execute_batch(
            "CREATE TABLE loops (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                workdir TEXT NOT NULL,
                status TEXT NOT NULL,
                trigger_type TEXT,
                trigger_config TEXT,
                created_at INTEGER NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                autorun_at INTEGER
             );
             CREATE TABLE loop_specs (
                id TEXT PRIMARY KEY,
                loop_id TEXT NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                description TEXT,
                position INTEGER NOT NULL,
                parallelizable INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                started_at INTEGER,
                completed_at INTEGER
             );
             INSERT INTO loops (id, name, workdir, status, created_at)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'draft', 0);
             INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES ('legacy-spec', 'legacy-loop', 'Spec', 1, 'pending');",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must rebuild the
    // table without losing the pre-existing, loop-bound row.
    let db = Database::new(&path).expect("open db, running migration");
    let spec = db.get_loop_spec("legacy-spec").unwrap().unwrap();
    assert_eq!(spec.loop_id.as_deref(), Some("legacy-loop"));
    assert_eq!(spec.workdir, None);
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent).
    let db = Database::new(&path).expect("reopen db after migration already applied");
    let spec = db.get_loop_spec("legacy-spec").unwrap().unwrap();
    assert_eq!(spec.loop_id.as_deref(), Some("legacy-loop"));
    assert_eq!(
        db.list_loop_specs("legacy-loop").unwrap().len(),
        1,
        "loop-bound spec must survive the rebuild"
    );

    // The rebuilt table now supports standalone specs (no loop) too.
    db.insert_loop_spec(&LoopSpec {
        id: "standalone-spec".to_string(),
        loop_id: None,
        name: "Backlog item".to_string(),
        description: Some("Do a thing".to_string()),
        position: 0,
        parallelizable: false,
        status: LoopSpecStatus::Pending,
        started_at: None,
        completed_at: None,
        spec_start_head: None,
        workdir: Some("/tmp/project".to_string()),
        completed_via: None,
        completed_via_reason: None,
        completed_via_at: None,
    })
    .unwrap();
    let standalone = db.get_loop_spec("standalone-spec").unwrap().unwrap();
    assert_eq!(standalone.loop_id, None);
    assert_eq!(standalone.workdir.as_deref(), Some("/tmp/project"));
}

fn sample_standalone_spec(id: &str, workdir: Option<&str>) -> LoopSpec {
    LoopSpec {
        id: id.to_string(),
        loop_id: None,
        name: format!("Backlog {id}"),
        description: Some(
            "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
        ),
        position: 0,
        parallelizable: false,
        status: LoopSpecStatus::Pending,
        started_at: None,
        completed_at: None,
        spec_start_head: None,
        workdir: workdir.map(str::to_string),
        completed_via: None,
        completed_via_reason: None,
        completed_via_at: None,
    }
}

#[test]
fn standalone_spec_crud_round_trip() {
    let db = test_db();
    let spec = sample_standalone_spec("backlog-1", Some("/tmp/project-a"));
    db.insert_loop_spec(&spec).unwrap();

    let fetched = db.get_loop_spec("backlog-1").unwrap().unwrap();
    assert_eq!(fetched.loop_id, None);
    assert_eq!(fetched.workdir.as_deref(), Some("/tmp/project-a"));
    assert_eq!(fetched.name, "Backlog backlog-1");

    let updated = db
        .update_spec_tag_details(
            "backlog-1",
            Some("Renamed"),
            None,
            Some(Some("/tmp/project-b")),
        )
        .unwrap();
    assert!(updated);
    let fetched = db.get_loop_spec("backlog-1").unwrap().unwrap();
    assert_eq!(fetched.name, "Renamed");
    assert_eq!(fetched.workdir.as_deref(), Some("/tmp/project-b"));

    let deleted = db.delete_loop_spec("backlog-1").unwrap();
    assert!(deleted);
    assert!(db.get_loop_spec("backlog-1").unwrap().is_none());
}

#[test]
fn list_specs_filters_by_workdir_and_unassigned_only() {
    let db = test_db();
    let lp = sample_loop("wf-backlog");
    db.insert_loop(&lp).unwrap();
    let bound_spec = sample_loop_spec(&lp.id, "bound-spec", 1);
    db.insert_loop_spec(&bound_spec).unwrap();

    let standalone_a = sample_standalone_spec("standalone-a", Some("/tmp/project-a"));
    let standalone_b = sample_standalone_spec("standalone-b", Some("/tmp/project-b"));
    db.insert_loop_spec(&standalone_a).unwrap();
    db.insert_loop_spec(&standalone_b).unwrap();

    // No filters: every spec, bound or not.
    let all = db.list_specs(None, None, false).unwrap();
    assert_eq!(all.len(), 3);

    // Filter by workdir: only the matching standalone spec.
    let by_workdir = db.list_specs(Some("/tmp/project-a"), None, false).unwrap();
    assert_eq!(by_workdir.len(), 1);
    assert_eq!(by_workdir[0].id, "standalone-a");

    // unassigned_only excludes the loop-bound spec.
    let unassigned = db.list_specs(None, None, true).unwrap();
    assert_eq!(unassigned.len(), 2);
    assert!(unassigned.iter().all(|s| s.loop_id.is_none()));
    assert!(unassigned.iter().any(|s| s.id == "standalone-a"));
    assert!(unassigned.iter().any(|s| s.id == "standalone-b"));

    // Filter by status: none of these are running.
    let running = db
        .list_specs(None, Some(LoopSpecStatus::Running), false)
        .unwrap();
    assert!(running.is_empty());
    let pending = db
        .list_specs(None, Some(LoopSpecStatus::Pending), false)
        .unwrap();
    assert_eq!(pending.len(), 3);
}

#[test]
fn loop_run_roundtrip_preserves_json_payloads() {
    let db = test_db();
    let lp = sample_loop("wf-2");
    let spec = sample_loop_spec(&lp.id, "spec-run", 1);
    let node = sample_loop_node(&spec.id, "node-run", 1);
    let run = LoopNodeRun {
        id: "run-1".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Pass,
        input: Some(serde_json::json!({"feedback": "previous"})),
        output: Some(serde_json::json!({"summary": "ok"})),
        started_at: Utc::now(),
        completed_at: Some(Utc::now()),
        iteration: 2,
        pid: Some(4242),
        boot_id: Some("boot-abc".to_string()),
        session_id: None,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();

    let runs = db.list_loop_runs_for_spec(&spec.id).unwrap();

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].iteration, 2);
    assert_eq!(runs[0].status, LoopRunStatus::Pass);
    assert_eq!(runs[0].pid, Some(4242));
    assert_eq!(runs[0].boot_id.as_deref(), Some("boot-abc"));
    assert_eq!(
        runs[0]
            .input
            .as_ref()
            .and_then(|value| value.get("feedback")),
        Some(&serde_json::json!("previous"))
    );
    assert_eq!(
        runs[0]
            .output
            .as_ref()
            .and_then(|value| value.get("summary")),
        Some(&serde_json::json!("ok"))
    );
}

#[test]
fn loop_updates_persist_metadata_and_positions() {
    let db = test_db();
    let lp = sample_loop("wf-update");
    let spec = sample_loop_spec(&lp.id, "spec-update", 1);
    let node = sample_loop_node(&spec.id, "node-update", 1);
    let edge = LoopEdge {
        id: "edge-update".to_string(),
        spec_id: Some(spec.id.clone()),
        loop_id: None,
        from_node: node.id.clone(),
        to_node: node.id.clone(),
        condition: LoopEdgeCondition::Always,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_edge(&edge).unwrap();

    db.update_loop_details(
        &lp.id,
        Some("Auth refresh loop"),
        Some(Some("Updated description")),
        Some("/tmp/other-project"),
    )
    .unwrap();
    db.update_loop_spec_details(
        &spec.id,
        Some("Spec updated"),
        Some("Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G"),
        Some(3),
        Some(true),
    )
    .unwrap();
    db.update_loop_node_details(
        &node.id,
        Some("Verification node"),
        Some(LoopNodeKind::Gate),
        Some(&serde_json::json!({"evaluate": "output_contains", "value": "APPROVED"})),
        Some(4),
    )
    .unwrap();
    db.update_loop_edge_condition(&edge.id, LoopEdgeCondition::Fail)
        .unwrap();

    let lp = db.get_loop(&lp.id).unwrap().unwrap();
    let spec = db.get_loop_spec(&spec.id).unwrap().unwrap();
    let node = db.get_loop_node(&node.id).unwrap().unwrap();
    let edge = db.get_loop_edge(&edge.id).unwrap().unwrap();

    assert_eq!(lp.name, "Auth refresh loop");
    assert_eq!(lp.description.as_deref(), Some("Updated description"));
    assert_eq!(lp.workdir, "/tmp/other-project");
    assert_eq!(spec.name, "Spec updated");
    assert_eq!(spec.position, 3);
    assert!(spec.parallelizable);
    assert_eq!(node.name, "Verification node");
    assert_eq!(node.kind, LoopNodeKind::Gate);
    assert_eq!(node.position, 4);
    assert_eq!(
        node.config.get("evaluate"),
        Some(&serde_json::json!("output_contains"))
    );
    assert_eq!(edge.condition, LoopEdgeCondition::Fail);
}

#[test]
fn loop_spec_start_head_persists_through_reread() {
    // G4: spec_start_head must survive a fresh read from the DB (e.g. after a
    // daemon restart), not just live on the in-memory LoopSpec that set it.
    let db = test_db();
    let lp = sample_loop("wf-start-head");
    let spec = sample_loop_spec(&lp.id, "spec-start-head", 1);

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();

    let fresh = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(fresh.spec_start_head, None);

    assert!(db
        .set_loop_spec_start_head(&spec.id, Some("e1c134b"))
        .unwrap());

    let reread = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(reread.spec_start_head.as_deref(), Some("e1c134b"));

    let listed = db
        .list_loop_specs(&lp.id)
        .unwrap()
        .into_iter()
        .find(|item| item.id == spec.id)
        .unwrap();
    assert_eq!(listed.spec_start_head.as_deref(), Some("e1c134b"));

    assert!(db.set_loop_spec_start_head(&spec.id, None).unwrap());
    let cleared = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(cleared.spec_start_head, None);
}

#[test]
fn reconcile_orphaned_loops_pauses_running_loop_and_interrupts_its_run() {
    let db = test_db();
    let mut lp = sample_loop("wf-orphan");
    lp.status = LoopStatus::Running;
    let mut spec = sample_loop_spec(&lp.id, "spec-orphan", 1);
    spec.status = LoopSpecStatus::Running;
    let node = sample_loop_node(&spec.id, "node-orphan", 1);
    let run = LoopNodeRun {
        id: "run-orphan".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: None,
        boot_id: None,
        session_id: None,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();
    // Prove the reconcile pass actually clears these, not that they were
    // never set.
    db.update_loop_spec_status(&spec.id, LoopSpecStatus::Running, Some(Utc::now()), None)
        .unwrap();
    db.set_loop_spec_start_head(&spec.id, Some("deadbeef"))
        .unwrap();

    let reconciled = db.reconcile_orphaned_loops().unwrap();
    assert_eq!(reconciled, 1);

    // Test 1: the loop is paused, and its dangling run is no longer `running`.
    let lp_after = db.get_loop(&lp.id).unwrap().unwrap();
    assert_eq!(lp_after.status, LoopStatus::Paused);
    let run_after = db.get_loop_run(&run.id).unwrap().unwrap();
    assert_ne!(run_after.status, LoopRunStatus::Running);
    assert_eq!(
        run_after
            .output
            .as_ref()
            .and_then(|value| value.get("interrupted")),
        Some(&serde_json::json!(true))
    );

    // Test 2 (B18): the spec is reset back to `pending` in the same pass —
    // its completed work is preserved by the worktree/commits, not by its
    // status, and leaving it `running` would make it invisible to pool
    // selection (`pool_next_pending_spec_id` only ever picks `pending`).
    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Pending);
    assert_eq!(spec_after.started_at, None);
    assert_eq!(spec_after.spec_start_head, None);
}

/// B18: the real incident — a pool-driven run's in-flight member (a
/// standalone spec, `loop_id: None`, never bound to the loop that's
/// currently running it) must be reset to `pending` exactly like a
/// loop-bound spec is. Left `running`, it would be invisible to
/// `pool_next_pending_spec_id` (which only ever picks `pending` members)
/// forever — the orphan this whole fix exists to prevent.
#[test]
fn reconcile_orphaned_loops_resets_pool_member_spec_to_pending() {
    let db = test_db();
    let mut lp = sample_loop("wf-orphan-pool");
    lp.status = LoopStatus::Running;
    lp.active_run_pool_id = Some("pool-1".to_string());
    db.insert_loop(&lp).unwrap();

    let mut spec = sample_loop_spec("unused-loop-id", "spec-orphan-pool", 1);
    spec.loop_id = None; // pool membership never binds the spec to a loop
    spec.status = LoopSpecStatus::Running;
    db.insert_loop_spec(&spec).unwrap();
    db.insert_pool(&Pool {
        id: "pool-1".to_string(),
        name: "pool-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_pool_member("pool-1", &spec.id).unwrap();

    let node = sample_loop_node(&spec.id, "node-orphan-pool", 1);
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&LoopNodeRun {
        id: "run-orphan-pool".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id,
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: None,
        boot_id: None,
        session_id: None,
    })
    .unwrap();

    assert_eq!(db.reconcile_orphaned_loops().unwrap(), 1);

    let lp_after = db.get_loop(&lp.id).unwrap().unwrap();
    assert_eq!(lp_after.status, LoopStatus::Paused);
    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Pending);
    // The pool's live pick can now find it again.
    assert_eq!(
        db.pool_next_pending_spec_id("pool-1").unwrap().as_deref(),
        Some(spec.id.as_str())
    );
}

/// R3 (B18): the defensive selection safety net. A pool member left
/// `running` with no `loop_runs` row proving it's still live in *this*
/// daemon's lifetime must be flagged as stale — but a member whose `running`
/// node run really does carry the current boot id (i.e. genuinely still in
/// flight right now) must not be.
#[test]
fn pool_stale_running_members_flags_only_the_member_with_no_live_run() {
    let db = test_db();
    let lp = sample_loop("wf-pool-stale");
    db.insert_loop(&lp).unwrap();

    let mut stale = sample_loop_spec("unused-loop-id", "spec-stale", 1);
    stale.loop_id = None;
    stale.status = LoopSpecStatus::Running;
    let mut live = sample_loop_spec("unused-loop-id", "spec-live", 2);
    live.loop_id = None;
    live.status = LoopSpecStatus::Running;
    db.insert_loop_spec(&stale).unwrap();
    db.insert_loop_spec(&live).unwrap();

    db.insert_pool(&Pool {
        id: "pool-1".to_string(),
        name: "pool-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_pool_member("pool-1", &stale.id).unwrap();
    db.append_pool_member("pool-1", &live.id).unwrap();

    // `live`'s node run genuinely belongs to the current daemon's boot.
    let node = sample_loop_node(&live.id, "node-live", 1);
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&LoopNodeRun {
        id: "run-live".to_string(),
        loop_id: lp.id,
        spec_id: live.id,
        node_id: node.id,
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: None,
        boot_id: Some("boot-current".to_string()),
        session_id: None,
    })
    .unwrap();

    let stale_members = db
        .pool_stale_running_members("pool-1", Some("boot-current"))
        .unwrap();
    assert_eq!(stale_members, vec![stale.id]);
}

#[test]
fn reconcile_orphaned_loops_is_idempotent() {
    let db = test_db();
    let mut lp = sample_loop("wf-orphan-idempotent");
    lp.status = LoopStatus::Running;
    let mut spec = sample_loop_spec(&lp.id, "spec-orphan-idempotent", 1);
    spec.status = LoopSpecStatus::Running;
    let node = sample_loop_node(&spec.id, "node-orphan-idempotent", 1);
    let run = LoopNodeRun {
        id: "run-orphan-idempotent".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: None,
        boot_id: None,
        session_id: None,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();

    let first_pass = db.reconcile_orphaned_loops().unwrap();
    assert_eq!(first_pass, 1);
    let lp_after_first = db.get_loop(&lp.id).unwrap().unwrap();
    let run_after_first = db.get_loop_run(&run.id).unwrap().unwrap();

    // Test 3: a second reconcile pass finds nothing left to reconcile, and
    // leaves the already-paused loop/run untouched.
    let second_pass = db.reconcile_orphaned_loops().unwrap();
    assert_eq!(second_pass, 0);
    let lp_after_second = db.get_loop(&lp.id).unwrap().unwrap();
    let run_after_second = db.get_loop_run(&run.id).unwrap().unwrap();
    assert_eq!(lp_after_second.status, lp_after_first.status);
    assert_eq!(run_after_second.status, run_after_first.status);
    assert_eq!(run_after_second.completed_at, run_after_first.completed_at);
}

/// B12: reconciliation at daemon boot can't have held a `Child` for a run
/// that predates it, but if the dangling run's `pid`/`boot_id` were
/// persisted by the process that spawned it, and the machine hasn't
/// rebooted since (same `boot_id`), reconciliation should still attempt a
/// best-effort kill of the survivor instead of just abandoning it.
#[tokio::test]
async fn reconcile_orphaned_loops_kills_survivor_pid_from_same_boot() {
    let Some(current_boot_id) = crate::system::boot_id() else {
        // Non-Linux host (or /proc unavailable): boot_id is never known, so
        // the same-boot check can never match — nothing to test here.
        return;
    };

    let db = test_db();
    let mut lp = sample_loop("wf-orphan-survivor");
    lp.status = LoopStatus::Running;
    let mut spec = sample_loop_spec(&lp.id, "spec-orphan-survivor", 1);
    spec.status = LoopSpecStatus::Running;
    let node = sample_loop_node(&spec.id, "node-orphan-survivor", 1);

    // A real, still-running process group leader to stand in for a `mimo
    // run` that outlived the daemon that spawned it. Must be its own
    // process-group leader (as every real spawn site is, via
    // `.process_group(0)`) for `killpg` to reach it rather than the test
    // process's own group.
    let mut command = std::process::Command::new("sleep");
    command.arg("30");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().expect("spawn survivor process");
    let pid = child.id() as i64;

    let run = LoopNodeRun {
        id: "run-orphan-survivor".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: Some(pid),
        boot_id: Some(current_boot_id),
        session_id: None,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();

    assert_eq!(db.reconcile_orphaned_loops().unwrap(), 1);

    // The kill is fired via a detached task (see
    // `terminate_process_group_async`); poll briefly for the SIGTERM to
    // land instead of asserting immediately.
    let killed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(killed, "survivor process from the same boot must be killed");
}

/// The mirror case: a dangling run whose `boot_id` does NOT match the
/// current machine boot must be left alone — the pid may have been recycled
/// by an unrelated process since the reboot, so killing it would be
/// dangerous, not just useless.
#[test]
fn reconcile_orphaned_loops_skips_kill_for_mismatched_boot_id() {
    let db = test_db();
    let mut lp = sample_loop("wf-orphan-stale-boot");
    lp.status = LoopStatus::Running;
    let mut spec = sample_loop_spec(&lp.id, "spec-orphan-stale-boot", 1);
    spec.status = LoopSpecStatus::Running;
    let node = sample_loop_node(&spec.id, "node-orphan-stale-boot", 1);
    let run = LoopNodeRun {
        id: "run-orphan-stale-boot".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        // A pid from a previous boot — never a real live process on this
        // machine right now, but also never allowed to be signaled.
        pid: Some(1),
        boot_id: Some("some-other-boot-that-is-not-current".to_string()),
        session_id: None,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();

    // Must not panic or error even though pid 1 is a real (unkillable by
    // us) process — the boot_id mismatch must short-circuit before any
    // signal is ever attempted.
    assert_eq!(db.reconcile_orphaned_loops().unwrap(), 1);
    let run_after = db.get_loop_run(&run.id).unwrap().unwrap();
    assert_ne!(run_after.status, LoopRunStatus::Running);
}

#[test]
fn reconcile_orphaned_loops_leaves_completed_loop_untouched() {
    let db = test_db();
    let mut lp = sample_loop("wf-completed");
    lp.status = LoopStatus::Completed;
    db.insert_loop(&lp).unwrap();

    // Test 4: a `Completed` loop is not reconciled.
    let reconciled = db.reconcile_orphaned_loops().unwrap();
    assert_eq!(reconciled, 0);
    let lp_after = db.get_loop(&lp.id).unwrap().unwrap();
    assert_eq!(lp_after.status, LoopStatus::Completed);
}

fn loop_with_trigger(id: &str, trigger: Option<Trigger>) -> Loop {
    Loop {
        trigger,
        ..sample_loop(id)
    }
}

#[test]
fn loop_trigger_round_trips_through_insert_and_get() {
    let db = test_db();
    let cron = loop_with_trigger(
        "wf-cron",
        Some(Trigger::Cron {
            schedule_expr: "30 8 * * *".to_string(),
        }),
    );
    db.insert_loop(&cron).unwrap();

    let fetched = db.get_loop("wf-cron").unwrap().unwrap();
    assert_eq!(fetched.schedule_expr(), Some("30 8 * * *"));
    assert!(fetched.is_cron());
}

fn sample_pool(id: &str) -> Pool {
    Pool {
        id: id.to_string(),
        name: format!("{id} name"),
        created_at: Utc::now(),
    }
}

#[test]
fn pool_and_members_round_trip_through_insert_and_get_details() {
    let db = test_db();
    for id in ["spec-a", "spec-b", "spec-c"] {
        db.insert_loop_spec(&sample_standalone_spec(id, None))
            .unwrap();
    }
    db.insert_pool(&sample_pool("pool-1")).unwrap();

    db.append_pool_member("pool-1", "spec-a").unwrap();
    db.append_pool_member("pool-1", "spec-b").unwrap();
    db.append_pool_member("pool-1", "spec-c").unwrap();
    assert_eq!(
        db.list_pool_member_spec_ids("pool-1").unwrap(),
        vec!["spec-a", "spec-b", "spec-c"]
    );
    assert!(db.pool_has_member("pool-1", "spec-b").unwrap());

    let details = db.get_pool_details("pool-1").unwrap().unwrap();
    assert_eq!(details.pool.name, "pool-1 name");
    assert_eq!(
        details
            .members
            .iter()
            .map(|spec| spec.id.clone())
            .collect::<Vec<_>>(),
        vec!["spec-a", "spec-b", "spec-c"]
    );

    assert!(db.remove_pool_member("pool-1", "spec-b").unwrap());
    assert!(!db.pool_has_member("pool-1", "spec-b").unwrap());
    assert_eq!(
        db.list_pool_member_spec_ids("pool-1").unwrap(),
        vec!["spec-a", "spec-c"]
    );

    let names = db
        .list_pools()
        .unwrap()
        .into_iter()
        .map(|pool| pool.id)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["pool-1"]);
    assert!(db.get_pool("does-not-exist").unwrap().is_none());
}

#[test]
fn reorder_pool_members_replaces_positions_in_given_order() {
    let db = test_db();
    for id in ["spec-a", "spec-b", "spec-c"] {
        db.insert_loop_spec(&sample_standalone_spec(id, None))
            .unwrap();
    }
    db.insert_pool(&sample_pool("pool-1")).unwrap();
    for id in ["spec-a", "spec-b", "spec-c"] {
        db.append_pool_member("pool-1", id).unwrap();
    }

    let order = vec![
        "spec-c".to_string(),
        "spec-a".to_string(),
        "spec-b".to_string(),
    ];
    db.reorder_pool_members("pool-1", &order).unwrap();

    assert_eq!(db.list_pool_member_spec_ids("pool-1").unwrap(), order);
}

#[test]
fn append_pool_member_rejects_nonexistent_spec() {
    let db = test_db();
    db.insert_pool(&sample_pool("pool-1")).unwrap();

    let error = db.append_pool_member("pool-1", "ghost-spec").unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("foreign key"),
        "{error}"
    );
}

#[test]
fn deleting_a_spec_cascades_its_pool_membership() {
    let db = test_db();
    db.insert_loop_spec(&sample_standalone_spec("spec-a", None))
        .unwrap();
    db.insert_pool(&sample_pool("pool-1")).unwrap();
    db.append_pool_member("pool-1", "spec-a").unwrap();

    db.delete_loop_spec("spec-a").unwrap();

    assert!(db.list_pool_member_spec_ids("pool-1").unwrap().is_empty());
}

#[test]
fn pools_migration_is_idempotent_and_a_pre_r4_database_opens_cleanly() {
    // Simulate a pre-R4 database: `loops` has the old `spec_pool` column
    // (7f2efdf) but no `pools`/`pool_members` tables at all.
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    {
        let conn = rusqlite::Connection::open(&path).expect("open raw legacy db");
        conn.execute_batch(
            "CREATE TABLE loops (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                workdir TEXT NOT NULL,
                status TEXT NOT NULL,
                trigger_type TEXT,
                trigger_config TEXT,
                created_at INTEGER NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                autorun_at INTEGER,
                spec_pool TEXT
             );
             CREATE TABLE loop_specs (
                id TEXT PRIMARY KEY,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                description TEXT,
                position INTEGER NOT NULL,
                parallelizable INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                workdir TEXT
             );
             INSERT INTO loops (id, name, workdir, status, created_at, spec_pool)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'draft', 0, NULL);
             INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES ('legacy-spec', 'legacy-loop', 'Spec', 1, 'pending');",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must succeed and add
    // the pools tables without disturbing existing rows.
    let db = Database::new(&path).expect("open pre-R4 db, running migration");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.name, "Legacy");
    db.insert_pool(&sample_pool("pool-1")).unwrap();
    db.append_pool_member("pool-1", "legacy-spec").unwrap();
    assert_eq!(
        db.list_pool_member_spec_ids("pool-1").unwrap(),
        vec!["legacy-spec"]
    );
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent).
    let db = Database::new(&path).expect("reopen db after migration already applied");
    assert_eq!(
        db.list_pool_member_spec_ids("pool-1").unwrap(),
        vec!["legacy-spec"]
    );

    // The retired `spec_pool` column is never written by current code: a
    // freshly inserted loop leaves it NULL.
    db.insert_loop(&sample_loop("fresh-loop")).unwrap();
    let raw: Option<String> = db
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT spec_pool FROM loops WHERE id = 'fresh-loop'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, None);
}

#[test]
fn active_run_pool_id_migration_is_idempotent_and_a_pre_b8_database_opens_cleanly() {
    // Simulate a pre-B8 database: `loops` has `autorun_at` and `pools`/
    // `pool_members` already exist, but `loops` predates `active_run_pool_id`.
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    {
        let conn = rusqlite::Connection::open(&path).expect("open raw legacy db");
        conn.execute_batch(
            "CREATE TABLE loops (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                workdir TEXT NOT NULL,
                status TEXT NOT NULL,
                trigger_type TEXT,
                trigger_config TEXT,
                created_at INTEGER NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                autorun_at INTEGER,
                spec_pool TEXT
             );
             CREATE TABLE loop_specs (
                id TEXT PRIMARY KEY,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                description TEXT,
                position INTEGER NOT NULL,
                parallelizable INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                workdir TEXT
             );
             CREATE TABLE pools (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE pool_members (
                pool_id TEXT NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
                spec_id TEXT NOT NULL REFERENCES loop_specs(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                PRIMARY KEY (pool_id, spec_id)
             );
             INSERT INTO loops (id, name, workdir, status, created_at)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'failed', 0);
             INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES ('legacy-spec', NULL, 'Spec', 1, 'pending');
             INSERT INTO pools (id, name, created_at) VALUES ('pool-1', 'pool-1', 0);
             INSERT INTO pool_members (pool_id, spec_id, position)
                 VALUES ('pool-1', 'legacy-spec', 1);",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must succeed and add
    // `active_run_pool_id` without disturbing existing rows.
    let db = Database::new(&path).expect("open pre-B8 db, running migration");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.name, "Legacy");
    assert_eq!(lp.active_run_pool_id, None);

    // The new column is actually usable: persist a run context and reset
    // through the shared path picks up the pool's members.
    db.set_loop_active_run_pool("legacy-loop", Some("pool-1"))
        .unwrap();
    let outcome = db.reset_loop("legacy-loop", None).unwrap();
    assert_eq!(
        outcome,
        crate::domain::loops::LoopResetOutcome::Reset { spec_count: 1 }
    );
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent).
    let db = Database::new(&path).expect("reopen db after migration already applied");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.active_run_pool_id.as_deref(), Some("pool-1"));
}

#[test]
fn list_cron_and_watch_loops_filter_by_trigger_type() {
    let db = test_db();
    let cron = loop_with_trigger(
        "wf-cron",
        Some(Trigger::Cron {
            schedule_expr: "0 9 * * *".to_string(),
        }),
    );
    let watch = loop_with_trigger(
        "wf-watch",
        Some(Trigger::Watch {
            path: "/tmp/watch".to_string(),
            events: vec![WatchEvent::Create],
            debounce_seconds: 2,
            recursive: false,
        }),
    );
    let manual = loop_with_trigger("wf-manual", None);

    db.insert_loop(&cron).unwrap();
    db.insert_loop(&watch).unwrap();
    db.insert_loop(&manual).unwrap();

    let cron_loops = db.list_cron_loops().unwrap();
    assert_eq!(cron_loops.len(), 1);
    assert_eq!(cron_loops[0].id, "wf-cron");

    let watch_loops = db.list_watch_loops().unwrap();
    assert_eq!(watch_loops.len(), 1);
    assert_eq!(watch_loops[0].id, "wf-watch");
    assert_eq!(watch_loops[0].watch_path(), Some("/tmp/watch"));

    // A manual loop appears in neither trigger list — it never self-fires.
    assert!(!cron_loops.iter().any(|lp| lp.id == "wf-manual"));
    assert!(!watch_loops.iter().any(|lp| lp.id == "wf-manual"));
}

#[test]
fn update_loop_trigger_sets_and_clears() {
    let db = test_db();
    let manual = loop_with_trigger("wf-swap", None);
    db.insert_loop(&manual).unwrap();
    assert!(db.list_cron_loops().unwrap().is_empty());

    // Set a cron trigger.
    db.update_loop_trigger(
        "wf-swap",
        Some(&Trigger::Cron {
            schedule_expr: "15 6 * * *".to_string(),
        }),
    )
    .unwrap();
    let cron_loops = db.list_cron_loops().unwrap();
    assert_eq!(cron_loops.len(), 1);
    assert_eq!(cron_loops[0].schedule_expr(), Some("15 6 * * *"));

    // Clear it back to manual.
    db.update_loop_trigger("wf-swap", None).unwrap();
    assert!(db.list_cron_loops().unwrap().is_empty());
    assert_eq!(
        db.get_loop("wf-swap")
            .unwrap()
            .unwrap()
            .trigger_type_label(),
        "manual"
    );
}

#[test]
fn test_list_cross_project_dependencies_returns_only_project_links() {
    let db = test_db();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("project-b".to_string()),
        kind: "project".to_string(),
        title: "Project B".to_string(),
        body: "B".to_string(),
        metadata: None,
        project_hash: Some("hash-b".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("project-a".to_string()),
        kind: "project".to_string(),
        title: "Project A".to_string(),
        body: "A".to_string(),
        metadata: None,
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: Some(vec![IntelligenceRelationInput {
            to_node_id: "project-b".to_string(),
            relation: "depends_on".to_string(),
            weight: Some(1.0),
        }]),
    })
    .unwrap();
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("fact-1".to_string()),
        kind: "fact".to_string(),
        title: "Fact".to_string(),
        body: "Fact body".to_string(),
        metadata: None,
        project_hash: None,
        session_id: None,
        relations: Some(vec![IntelligenceRelationInput {
            to_node_id: "project-a".to_string(),
            relation: "depends_on".to_string(),
            weight: Some(1.0),
        }]),
    })
    .unwrap();

    let deps = db.list_cross_project_dependencies(10).unwrap();

    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].from_node_id, "project-a");
    assert_eq!(deps[0].to_node_id, "project-b");
    assert_eq!(deps[0].relation, "depends_on");
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
fn test_rag_queue_counts() {
    let db = test_db();
    db.enqueue_rag_item("/tmp/a.rs", 111).unwrap();
    db.enqueue_rag_item("/tmp/b.rs", 112).unwrap();
    db.mark_rag_item_processing("/tmp/a.rs", 113).unwrap();

    let (queued, processing) = db.rag_queue_counts().unwrap();
    assert_eq!(queued, 1);
    assert_eq!(processing, 1);
}

#[test]
fn test_requeue_processing_rag_items() {
    let db = test_db();
    db.enqueue_rag_item("/tmp/a.rs", 111).unwrap();
    db.enqueue_rag_item("/tmp/b.rs", 112).unwrap();
    db.mark_rag_item_processing("/tmp/a.rs", 113).unwrap();

    let recovered = db.requeue_processing_rag_items(999).unwrap();
    assert_eq!(recovered, 1);

    let items = db.list_rag_queue(10).unwrap();
    let a = items
        .iter()
        .find(|item| item.source_path == "/tmp/a.rs")
        .expect("requeued item exists");
    assert_eq!(a.status, "queued");
    assert_eq!(a.queued_at, 999);

    let (queued, processing) = db.rag_queue_counts().unwrap();
    assert_eq!(queued, 2);
    assert_eq!(processing, 0);
}

#[test]
fn test_indexed_files_timestamps_uses_last_success_unless_deleted() {
    let db = test_db();

    db.log_rag_event("/tmp/a.md", "indexed", None, 100).unwrap();
    db.log_rag_event("/tmp/a.md", "error", Some("transient"), 110)
        .unwrap();

    db.log_rag_event("/tmp/b.md", "indexed", None, 120).unwrap();
    db.log_rag_event("/tmp/b.md", "deleted", None, 130).unwrap();

    db.log_rag_event("/tmp/c.md", "indexed", None, 90).unwrap();
    db.log_rag_event("/tmp/c.md", "indexed", None, 140).unwrap();

    db.log_rag_event("/tmp/d.md", "error", Some("never indexed"), 150)
        .unwrap();

    let timestamps = db.indexed_files_timestamps().unwrap();

    assert_eq!(timestamps.get("/tmp/a.md"), Some(&100));
    assert_eq!(timestamps.get("/tmp/c.md"), Some(&140));
    assert!(!timestamps.contains_key("/tmp/b.md"));
    assert!(!timestamps.contains_key("/tmp/d.md"));
}

#[test]
fn test_rag_error_count_and_permanently_failed_files() {
    let db = test_db();

    db.log_rag_event("/tmp/bad.pdf", "error", Some("attempt 1"), 100)
        .unwrap();
    db.log_rag_event("/tmp/bad.pdf", "error", Some("attempt 2"), 110)
        .unwrap();
    db.log_rag_event("/tmp/bad.pdf", "error", Some("attempt 3"), 120)
        .unwrap();
    db.log_rag_event("/tmp/bad.pdf", "failed", Some("giving up"), 120)
        .unwrap();

    db.log_rag_event("/tmp/ok.md", "indexed", None, 50).unwrap();
    db.log_rag_event("/tmp/ok.md", "error", Some("transient"), 60)
        .unwrap();

    assert_eq!(db.rag_error_count("/tmp/bad.pdf").unwrap(), 3);
    assert_eq!(db.rag_error_count("/tmp/ok.md").unwrap(), 1);
    assert_eq!(db.rag_error_count("/tmp/unknown.md").unwrap(), 0);

    let failed = db.permanently_failed_rag_files().unwrap();
    assert!(failed.contains("/tmp/bad.pdf"));
    assert!(!failed.contains("/tmp/ok.md"));
}

#[test]
fn test_permanently_failed_rag_files_cleared_by_later_index() {
    let db = test_db();

    db.log_rag_event("/tmp/retry.pdf", "error", Some("attempt 1"), 100)
        .unwrap();
    db.log_rag_event("/tmp/retry.pdf", "failed", Some("giving up"), 100)
        .unwrap();

    let failed = db.permanently_failed_rag_files().unwrap();
    assert!(failed.contains("/tmp/retry.pdf"));

    // A manual re-add later succeeds — the file should no longer be
    // considered permanently failed.
    db.log_rag_event("/tmp/retry.pdf", "indexed", None, 200)
        .unwrap();

    let failed = db.permanently_failed_rag_files().unwrap();
    assert!(!failed.contains("/tmp/retry.pdf"));
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

    assert!(db.delete_agent("to-delete").unwrap(), "row existed");
    assert!(db.get_agent("to-delete").unwrap().is_none());
}

#[test]
fn test_delete_agent_reports_whether_a_row_existed() {
    let db = test_db();
    assert!(
        !db.delete_agent("never-existed").unwrap(),
        "deleting a missing id must report false, not error"
    );
}

/// B7: a single corrupt row (malformed `trigger_config`, e.g. a raw cron
/// string inserted directly via SQL by an external tool) must not take down
/// `list_agents`/`list_cron_agents`/`list_watch_agents` — it must simply be
/// absent from those healthy-only lists.
#[test]
fn list_queries_skip_corrupt_row_without_erroring() {
    let db = test_db();
    db.insert_corrupt_agent_for_test("corrupt-1", true).unwrap();
    db.upsert_agent(&sample_cron_agent("cron-1")).unwrap();
    db.upsert_agent(&sample_watch_agent("watch-1")).unwrap();

    let all = db
        .list_agents()
        .expect("a corrupt row must not error the whole query");
    let mut ids: Vec<&str> = all.iter().map(|a| a.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["cron-1", "watch-1"]);

    let cron = db.list_cron_agents().expect("must not error");
    assert_eq!(cron.len(), 1);
    assert_eq!(cron[0].id, "cron-1");

    let watch = db.list_watch_agents().expect("must not error");
    assert_eq!(watch.len(), 1);
    assert_eq!(watch[0].id, "watch-1");
}

/// `list_corrupt_agents` is the one place corrupt rows are surfaced —
/// callers (scheduler quarantine, TUI, MCP `agent_list`) use it to flag the
/// row instead of guessing at its contents.
#[test]
fn list_corrupt_agents_flags_the_row_with_its_parse_error() {
    let db = test_db();
    db.insert_corrupt_agent_for_test("corrupt-1", true).unwrap();
    db.upsert_agent(&sample_cron_agent("cron-1")).unwrap();

    let corrupt = db.list_corrupt_agents().unwrap();
    assert_eq!(corrupt.len(), 1);
    assert_eq!(corrupt[0].id, "corrupt-1");
    assert!(corrupt[0].enabled);
    assert!(
        !corrupt[0].error.is_empty(),
        "must carry the parse error for diagnosis"
    );
}

/// `agent_remove` must always succeed against a corrupt row — it deletes by
/// id and never has to parse `trigger_config` to do it.
#[test]
fn delete_agent_removes_corrupt_row_without_parsing_it() {
    let db = test_db();
    db.insert_corrupt_agent_for_test("corrupt-1", true).unwrap();

    assert!(db.delete_agent("corrupt-1").unwrap());
    assert!(db.list_corrupt_agents().unwrap().is_empty());
    assert!(db.list_agents().unwrap().is_empty());
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
fn test_schedule_agent_enable_persists_enable_at_and_stays_disabled() {
    let db = test_db();
    let mut agent = sample_cron_agent("wake-me");
    agent.enabled = false;
    db.upsert_agent(&agent).unwrap();

    let at = Utc::now() + Duration::hours(1);
    db.schedule_agent_enable("wake-me", at).unwrap();

    let agent = db.get_agent("wake-me").unwrap().unwrap();
    assert!(
        !agent.enabled,
        "scheduling enable must not enable immediately"
    );
    assert_eq!(agent.enable_at.map(|t| t.timestamp()), Some(at.timestamp()));
}

#[test]
fn test_activate_scheduled_enable_enables_and_clears_enable_at() {
    let db = test_db();
    let mut agent = sample_cron_agent("wake-me-2");
    agent.enabled = false;
    db.upsert_agent(&agent).unwrap();
    db.schedule_agent_enable("wake-me-2", Utc::now() - Duration::minutes(5))
        .unwrap();

    db.activate_scheduled_enable("wake-me-2").unwrap();

    let agent = db.get_agent("wake-me-2").unwrap().unwrap();
    assert!(agent.enabled, "activation must enable the agent");
    assert!(agent.enable_at.is_none(), "activation must clear enable_at");
}

#[test]
fn test_list_pending_enable_agents_filters_correctly() {
    let db = test_db();

    let mut pending = sample_cron_agent("pending-1");
    pending.enabled = false;
    db.upsert_agent(&pending).unwrap();
    db.schedule_agent_enable("pending-1", Utc::now() + Duration::hours(1))
        .unwrap();

    // Enabled agent with no enable_at — must not show up.
    db.upsert_agent(&sample_cron_agent("already-enabled"))
        .unwrap();

    // Disabled agent with no enable_at set — must not show up.
    let mut disabled_only = sample_cron_agent("disabled-only");
    disabled_only.enabled = false;
    db.upsert_agent(&disabled_only).unwrap();

    let results = db.list_pending_enable_agents().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "pending-1");
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

#[test]
fn test_rename_agent_updates_agent_and_run_references() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("old-name")).unwrap();
    let run = RunLog {
        id: uuid::Uuid::new_v4().to_string(),
        background_agent_id: "old-name".to_string(),
        status: RunStatus::Success,
        trigger_type: TriggerType::Scheduled,
        summary: None,
        started_at: Utc::now(),
        finished_at: Some(Utc::now()),
        exit_code: Some(0),
        timeout_at: None,
    };
    db.insert_run(&run).unwrap();

    db.rename_agent("old-name", "new-name", "/tmp/new-name.log")
        .unwrap();

    assert!(db.get_agent("old-name").unwrap().is_none());
    let renamed = db
        .get_agent("new-name")
        .unwrap()
        .expect("renamed agent exists under new id");
    assert_eq!(renamed.log_path, "/tmp/new-name.log");

    let runs = db.list_runs("new-name", 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);
    assert!(db.list_runs("old-name", 10).unwrap().is_empty());
}

#[test]
fn test_rename_agent_fails_when_new_id_already_exists() {
    let db = test_db();
    db.upsert_agent(&sample_cron_agent("agent-a")).unwrap();
    db.upsert_agent(&sample_cron_agent("agent-b")).unwrap();

    let result = db.rename_agent("agent-a", "agent-b", "/tmp/agent-b.log");
    assert!(result.is_err());

    // Neither agent should have been touched by the rejected rename.
    assert!(db.get_agent("agent-a").unwrap().is_some());
    let b = db.get_agent("agent-b").unwrap().unwrap();
    assert_eq!(b.log_path, "/tmp/test.log");
}

#[test]
fn test_rename_agent_fails_when_old_id_missing() {
    let db = test_db();
    let result = db.rename_agent("does-not-exist", "new-id", "/tmp/new-id.log");
    assert!(result.is_err());
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

    let snapshot = db
        .get_intelligence_node(&format!("run:{run_id}"))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.kind, "session");
    assert!(snapshot.body.contains("Done"));
    assert!(snapshot
        .metadata
        .as_deref()
        .unwrap_or_default()
        .contains("/tmp/project"));
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

// ── Intelligence V2: Project-Linked Knowledge ─────────────────────

#[test]
fn test_list_projects_returns_project_nodes() {
    let db = test_db();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-a".to_string()),
        kind: "project".to_string(),
        title: "Project Alpha".to_string(),
        body: "Alpha project description".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-a"})),
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("fact-1".to_string()),
        kind: "fact".to_string(),
        title: "Some fact".to_string(),
        body: "fact body".to_string(),
        metadata: None,
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    let projects = db.list_intelligence_projects(None, 10).unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, "proj-a");
    assert_eq!(projects[0].kind, "project");
}

#[test]
fn test_list_projects_filters_by_query() {
    let db = test_db();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-a".to_string()),
        kind: "project".to_string(),
        title: "Alpha Backend".to_string(),
        body: "Backend services".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-a"})),
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-b".to_string()),
        kind: "project".to_string(),
        title: "Beta Frontend".to_string(),
        body: "Frontend app".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-b"})),
        project_hash: Some("hash-b".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    let results = db.list_intelligence_projects(Some("frontend"), 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Beta Frontend");
}

#[test]
fn test_link_projects_creates_edge_between_projects() {
    let db = test_db();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-a".to_string()),
        kind: "project".to_string(),
        title: "Project A".to_string(),
        body: "A".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-a"})),
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-b".to_string()),
        kind: "project".to_string(),
        title: "Project B".to_string(),
        body: "B".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-b"})),
        project_hash: Some("hash-b".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    let edge = db
        .link_projects("hash-a", "hash-b", "depends_on", Some(2.0))
        .unwrap();
    assert_eq!(edge.relation, "depends_on");
    assert_eq!(edge.weight, 2.0);
    assert_eq!(edge.from_node_id, "proj-a");
    assert_eq!(edge.to_node_id, "proj-b");
}

#[test]
fn test_link_projects_fails_when_project_missing() {
    let db = test_db();
    let result = db.link_projects("nonexistent-a", "nonexistent-b", "relates_to", None);
    assert!(result.is_err());
}

#[test]
fn test_list_project_knowledge_returns_facts_and_patterns() {
    let db = test_db();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-a".to_string()),
        kind: "project".to_string(),
        title: "Project A".to_string(),
        body: "A".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-a"})),
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("fact-1".to_string()),
        kind: "fact".to_string(),
        title: "DB convention".to_string(),
        body: "Always use SQLite".to_string(),
        metadata: None,
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("pattern-1".to_string()),
        kind: "pattern".to_string(),
        title: "Error handling".to_string(),
        body: "Use anyhow".to_string(),
        metadata: None,
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("fact-other".to_string()),
        kind: "fact".to_string(),
        title: "Other fact".to_string(),
        body: "unrelated".to_string(),
        metadata: None,
        project_hash: Some("hash-other".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    let knowledge = db.list_project_knowledge("hash-a", None, 10).unwrap();
    assert_eq!(knowledge.len(), 2);

    let facts = db
        .list_project_knowledge("hash-a", Some("fact"), 10)
        .unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind, "fact");

    let patterns = db
        .list_project_knowledge("hash-a", Some("pattern"), 10)
        .unwrap();
    assert_eq!(patterns.len(), 1);
    assert_eq!(patterns[0].kind, "pattern");
}

#[test]
fn test_list_related_projects_finds_linked_projects() {
    let db = test_db();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-a".to_string()),
        kind: "project".to_string(),
        title: "Project A".to_string(),
        body: "A".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-a"})),
        project_hash: Some("hash-a".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-b".to_string()),
        kind: "project".to_string(),
        title: "Project B".to_string(),
        body: "B".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-b"})),
        project_hash: Some("hash-b".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-c".to_string()),
        kind: "project".to_string(),
        title: "Project C".to_string(),
        body: "C".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-c"})),
        project_hash: Some("hash-c".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    db.link_projects("hash-a", "hash-b", "depends_on", Some(2.0))
        .unwrap();
    db.link_projects("hash-a", "hash-c", "relates_to", Some(1.0))
        .unwrap();

    let related = db.list_related_projects("hash-a", 10).unwrap();
    assert_eq!(related.len(), 2);

    let titles: Vec<_> = related.iter().map(|(n, _)| n.title.as_str()).collect();
    assert!(titles.contains(&"Project B"));
    assert!(titles.contains(&"Project C"));
}

#[test]
fn test_list_related_projects_returns_empty_for_unlinked_project() {
    let db = test_db();

    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("proj-lone".to_string()),
        kind: "project".to_string(),
        title: "Lone Project".to_string(),
        body: "No relations".to_string(),
        metadata: Some(serde_json::json!({"hash": "hash-lone"})),
        project_hash: Some("hash-lone".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    let related = db.list_related_projects("hash-lone", 10).unwrap();
    assert!(related.is_empty());
}

// ── Seed session binding tests ──────────────────────────────────────

#[test]
fn seed_bind_and_resolve() {
    let db = test_db();
    db.insert_interactive_session(
        "session-abc",
        "test-session",
        "opencode",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();

    db.bind_session_to_seed("session-abc", "seed-oak").unwrap();
    let resolved = db.resolve_session_seed("session-abc").unwrap();
    assert_eq!(resolved, Some("seed-oak".to_string()));
}

#[test]
fn seed_resolve_missing_returns_none() {
    let db = test_db();

    let resolved = db.resolve_session_seed("nonexistent-session").unwrap();
    assert!(resolved.is_none());
}

#[test]
fn seed_bind_replaces_existing() {
    let db = test_db();
    db.insert_interactive_session(
        "session-abc",
        "test-session",
        "opencode",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();

    db.bind_session_to_seed("session-abc", "seed-oak").unwrap();
    db.bind_session_to_seed("session-abc", "seed-pine").unwrap();

    let resolved = db.resolve_session_seed("session-abc").unwrap();
    assert_eq!(resolved, Some("seed-pine".to_string()));
}

#[test]
fn seed_unbind_removes_binding() {
    let db = test_db();
    db.insert_interactive_session(
        "session-abc",
        "test-session",
        "opencode",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();

    db.bind_session_to_seed("session-abc", "seed-oak").unwrap();
    db.unbind_session_seed("session-abc").unwrap();

    let resolved = db.resolve_session_seed("session-abc").unwrap();
    assert!(resolved.is_none());
}

#[test]
fn seed_unbind_nonexistent_is_ok() {
    let db = test_db();

    let result = db.unbind_session_seed("nonexistent-session");
    assert!(result.is_ok());
}

#[test]
fn seed_get_sessions_for_seed_empty() {
    let db = test_db();

    let sessions = db.get_sessions_for_seed("seed-oak").unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn seed_multiple_sessions_for_same_seed() {
    let db = test_db();
    db.insert_interactive_session(
        "session-1",
        "s1",
        "opencode",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    db.insert_interactive_session(
        "session-2",
        "s2",
        "opencode",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    db.insert_interactive_session(
        "session-3",
        "s3",
        "opencode",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();

    db.bind_session_to_seed("session-1", "seed-oak").unwrap();
    db.bind_session_to_seed("session-2", "seed-oak").unwrap();
    db.bind_session_to_seed("session-3", "seed-pine").unwrap();

    // Verify bindings exist
    assert!(db.resolve_session_seed("session-1").unwrap().is_some());
    assert!(db.resolve_session_seed("session-2").unwrap().is_some());
    assert!(db.resolve_session_seed("session-3").unwrap().is_some());
}

#[test]
fn interactive_session_pid_round_trips_through_get_active_sessions() {
    let db = test_db();
    db.insert_interactive_session(
        "session-with-pid",
        "with-pid",
        "opencode",
        "/tmp",
        None,
        Some(4321),
        "interactive",
        None,
    )
    .unwrap();
    db.insert_interactive_session(
        "session-without-pid",
        "without-pid",
        "opencode",
        "/tmp",
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();

    let sessions = db.get_active_sessions().unwrap();

    let with_pid = sessions
        .iter()
        .find(|s| s.id == "session-with-pid")
        .expect("session-with-pid present");
    assert_eq!(with_pid.pid, Some(4321));

    let without_pid = sessions
        .iter()
        .find(|s| s.id == "session-without-pid")
        .expect("session-without-pid present");
    assert_eq!(without_pid.pid, None);
}

#[test]
fn get_active_sessions_excludes_bridge_sessions() {
    let db = test_db();
    db.insert_interactive_session(
        "bridge-session",
        "standalone",
        "bridge",
        "/tmp",
        Some("canopy bridge"),
        Some(4321),
        "bridge",
        None,
    )
    .unwrap();
    db.insert_interactive_session(
        "chat-session",
        "chat",
        "opencode",
        "/tmp",
        None,
        Some(1234),
        "interactive",
        None,
    )
    .unwrap();

    let sessions = db.get_active_sessions().unwrap();

    assert!(sessions.iter().all(|s| s.id != "bridge-session"));
    assert!(sessions.iter().any(|s| s.id == "chat-session"));
}

#[test]
fn get_active_sessions_by_type_returns_only_matching_bridge_rows() {
    let db = test_db();
    db.insert_interactive_session(
        "bridge-session",
        "standalone",
        "bridge",
        "/tmp",
        Some("canopy bridge"),
        Some(4321),
        "bridge",
        None,
    )
    .unwrap();
    db.insert_interactive_session(
        "chat-session",
        "chat",
        "opencode",
        "/tmp",
        None,
        Some(1234),
        "interactive",
        None,
    )
    .unwrap();

    let bridges = db.get_active_sessions_by_type("bridge").unwrap();

    assert_eq!(bridges.len(), 1);
    assert_eq!(bridges[0].id, "bridge-session");
}

#[test]
fn session_marking_is_per_row_not_a_mass_pre_pass() {
    // Simulates a crash partway through the auto-resume loop: two active
    // sessions exist, but only the first gets handled (marked orphaned)
    // before the "crash". The old `mark_orphaned_sessions` mass pre-pass
    // would have flipped both to 'orphaned' up front; the per-session
    // primitives must leave the untouched second session 'active'.
    let db = test_db();
    db.insert_interactive_session(
        "session-1",
        "first",
        "opencode",
        "/tmp",
        None,
        Some(111),
        "interactive",
        None,
    )
    .unwrap();
    db.insert_interactive_session(
        "session-2",
        "second",
        "opencode",
        "/tmp",
        None,
        Some(222),
        "interactive",
        None,
    )
    .unwrap();

    // Only the first session is handled before the simulated crash.
    db.mark_session_orphaned("session-1").unwrap();

    let active = db.get_active_sessions().unwrap();
    assert!(active.iter().all(|s| s.id != "session-1"));
    assert!(
        active.iter().any(|s| s.id == "session-2"),
        "untouched second session must still be 'active', not orphaned"
    );

    let orphaned = db.get_orphaned_sessions().unwrap();
    assert!(orphaned.iter().any(|s| s.id == "session-1"));
    assert!(orphaned.iter().all(|s| s.id != "session-2"));
}

#[test]
fn mark_session_orphaned_is_a_noop_once_already_resumed() {
    // Guards the "only transitions rows that are still 'active'" contract:
    // once a session has been marked 'resumed' it must not be flippable
    // back to 'orphaned' by a stray call.
    let db = test_db();
    db.insert_interactive_session(
        "session-1",
        "first",
        "opencode",
        "/tmp",
        None,
        Some(111),
        "interactive",
        None,
    )
    .unwrap();

    db.mark_session_resumed("session-1").unwrap();
    db.mark_session_orphaned("session-1").unwrap();

    let orphaned = db.get_orphaned_sessions().unwrap();
    assert!(orphaned.iter().all(|s| s.id != "session-1"));
}

#[test]
fn boot_id_migration_is_idempotent_and_a_pre_boot_id_database_opens_cleanly() {
    // Simulate a database written before the boot_id column existed:
    // interactive_sessions has `pid` but no `boot_id`.
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    {
        let conn = rusqlite::Connection::open(&path).expect("open raw legacy db");
        conn.execute_batch(
            "CREATE TABLE interactive_sessions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                cli TEXT NOT NULL,
                working_dir TEXT NOT NULL,
                args TEXT,
                started_at TEXT NOT NULL,
                exited_at TEXT,
                exit_code INTEGER,
                status TEXT NOT NULL DEFAULT 'active',
                session_type TEXT NOT NULL DEFAULT 'interactive',
                pid INTEGER
             );
             INSERT INTO interactive_sessions
                 (id, name, cli, working_dir, started_at, status, session_type, pid)
                 VALUES ('legacy-session', 'legacy', 'opencode', '/tmp', '2023-01-01T00:00:00Z', 'active', 'interactive', 4242);",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must succeed, add
    // the boot_id column, and leave the existing row queryable with a NULL
    // boot_id (legacy rows are always safe to resume — see should_resume_session).
    let db = Database::new(&path).expect("open pre-boot_id db, running migration");
    let sessions = db.get_active_sessions().unwrap();
    let legacy = sessions
        .iter()
        .find(|s| s.id == "legacy-session")
        .expect("legacy row still present after migration");
    assert_eq!(legacy.boot_id, None);
    assert_eq!(legacy.pid, Some(4242));
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent).
    let db = Database::new(&path).expect("reopen db after migration already applied");
    let sessions = db.get_active_sessions().unwrap();
    assert!(sessions.iter().any(|s| s.id == "legacy-session"));
}

#[test]
fn legacy_bridge_rows_are_reclassified_by_migration() {
    // Simulate a row left over from before session_type = 'bridge' existed:
    // old builds stored the bridge sidecar with session_type = 'interactive'.
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    let db = Database::new(&path).expect("create test db");
    db.insert_interactive_session(
        "legacy-bridge",
        "standalone",
        "bridge",
        "/tmp",
        Some("canopy bridge"),
        Some(4321),
        "interactive",
        None,
    )
    .unwrap();
    drop(db);

    // Re-running the migration (as happens on every Database::new) must
    // reclassify the legacy row, and running it again must be a no-op.
    drop(Database::new(&path).expect("reopen test db"));
    let db = Database::new(&path).expect("reopen test db again after no-op migration");

    assert_eq!(
        db.get_session_type("legacy-bridge").unwrap().as_deref(),
        Some("bridge")
    );
    assert!(db
        .get_active_sessions()
        .unwrap()
        .iter()
        .all(|s| s.id != "legacy-bridge"));
}

#[test]
fn registering_project_creates_intelligence_root_node() {
    let db = test_db();
    let dir_a = tempdir().expect("tempdir a");
    let dir_b = tempdir().expect("tempdir b");

    let a = db.register_project_path(dir_a.path()).expect("register a");
    let b = db.register_project_path(dir_b.path()).expect("register b");

    let projects = db
        .list_intelligence_projects(None, 10)
        .expect("list project nodes");
    assert_eq!(
        projects.len(),
        2,
        "each registered project gets a root node"
    );

    let edge = db
        .link_projects(&a.hash, &b.hash, "relates_to", None)
        .expect("link via hashes resolves the auto-created nodes");
    assert_eq!(edge.relation, "relates_to");

    let graph = db
        .walk_intelligence_graph(&format!("project:{}", a.hash), 2)
        .expect("walk")
        .expect("root exists");
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.id == format!("project:{}", b.hash)),
        "linked project is reachable from the root"
    );
}

#[test]
fn backfill_recreates_missing_project_nodes() {
    let db = test_db();
    let dir = tempdir().expect("tempdir");
    let project = db.register_project_path(dir.path()).expect("register");

    let node_id = format!("project:{}", project.hash);
    db.delete_intelligence_node(&node_id)
        .expect("simulate legacy db without project nodes");
    assert!(db
        .list_intelligence_projects(None, 10)
        .expect("list")
        .is_empty());

    let created = db.backfill_project_nodes().expect("backfill");
    assert_eq!(created, 1);
    assert_eq!(
        db.list_intelligence_projects(None, 10).expect("list").len(),
        1
    );
}

// ── B25: Administrative spec completion ──────────────────────────────

#[test]
fn set_spec_admin_status_transitions_each_status() {
    let db = test_db();

    for target_status in &[
        LoopSpecStatus::Completed,
        LoopSpecStatus::Skipped,
        LoopSpecStatus::Pending,
    ] {
        let mut spec = sample_loop_spec("unused", &format!("spec-{:?}", target_status), 1);
        spec.loop_id = None;
        db.insert_loop_spec(&spec).unwrap();

        let outcome = db
            .set_spec_admin_status(&spec.id, *target_status, "test reason")
            .unwrap();

        assert!(
            matches!(outcome, SpecAdminStatusOutcome::Success),
            "transition to {:?} failed",
            target_status
        );

        let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
        assert_eq!(spec_after.status, *target_status);
        assert_eq!(
            spec_after.completed_via,
            Some("admin".to_string()),
            "completed_via should be 'admin' for {:?}",
            target_status
        );
        assert_eq!(
            spec_after.completed_via_reason,
            Some("test reason".to_string()),
            "completed_via_reason should match for {:?}",
            target_status
        );

        if *target_status != LoopSpecStatus::Pending {
            assert!(
                spec_after.completed_via_at.is_some(),
                "completed_via_at should be set for {:?}",
                target_status
            );
        } else {
            assert!(
                spec_after.completed_via_at.is_none(),
                "completed_via_at should be None for Pending"
            );
        }
    }
}

#[test]
fn set_spec_admin_status_rejects_missing_spec() {
    let db = test_db();
    let outcome = db
        .set_spec_admin_status("nonexistent", LoopSpecStatus::Completed, "reason")
        .unwrap();
    assert!(
        matches!(outcome, SpecAdminStatusOutcome::NotFound),
        "should reject missing spec"
    );
}

#[test]
fn set_spec_admin_status_rejects_loop_bound_spec() {
    let db = test_db();
    let lp = sample_loop("loop-bound-test");
    db.insert_loop(&lp).unwrap();

    let spec = sample_loop_spec(&lp.id, "spec-bound", 1);
    db.insert_loop_spec(&spec).unwrap();

    let outcome = db
        .set_spec_admin_status(&spec.id, LoopSpecStatus::Completed, "reason")
        .unwrap();

    assert!(
        matches!(outcome, SpecAdminStatusOutcome::NotStandalone(ref id) if id == &lp.id),
        "should reject loop-bound spec"
    );
}

#[test]
fn set_spec_admin_status_rejects_active_run() {
    let db = test_db();
    let mut spec = sample_loop_spec("unused", "spec-with-run", 1);
    spec.loop_id = None;
    db.insert_loop_spec(&spec).unwrap();

    let lp = sample_loop("loop-for-run");
    db.insert_loop(&lp).unwrap();

    let node = sample_loop_node(&spec.id, "node-for-run", 1);
    db.insert_loop_node(&node).unwrap();

    let run = LoopNodeRun {
        id: "run-active".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id,
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: None,
        boot_id: None,
        session_id: None,
    };
    db.insert_loop_run(&run).unwrap();

    let outcome = db
        .set_spec_admin_status(&spec.id, LoopSpecStatus::Completed, "reason")
        .unwrap();

    assert!(
        matches!(outcome, SpecAdminStatusOutcome::ActiveRun { ref loop_id, ref run_id }
            if loop_id == &lp.id && run_id == "run-active"),
        "should reject spec with active run"
    );
}

#[test]
fn set_spec_admin_status_propagates_to_pool_selection() {
    let db = test_db();
    let mut spec = sample_loop_spec("unused", "spec-pool-prop", 1);
    spec.loop_id = None;
    db.insert_loop_spec(&spec).unwrap();

    let pool = Pool {
        id: "pool-test".to_string(),
        name: "pool-test".to_string(),
        created_at: Utc::now(),
    };
    db.insert_pool(&pool).unwrap();
    db.append_pool_member("pool-test", &spec.id).unwrap();

    let before = db.pool_next_pending_spec_id("pool-test").unwrap();
    assert_eq!(before.as_deref(), Some(spec.id.as_str()));

    let outcome = db
        .set_spec_admin_status(&spec.id, LoopSpecStatus::Completed, "reason")
        .unwrap();
    assert!(matches!(outcome, SpecAdminStatusOutcome::Success));

    let after = db.pool_next_pending_spec_id("pool-test").unwrap();
    assert_eq!(after, None, "pool should not select completed spec anymore");
}
