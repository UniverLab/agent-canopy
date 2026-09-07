use super::*;
use crate::db::intelligence::{IntelligenceNodeInput, IntelligenceRelationInput};
use crate::domain::loops::{
    Loop, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus,
    LoopSpec, LoopSpecStatus, LoopStatus, SpecAdminStatusOutcome,
};
use crate::domain::models::{Agent, Cli, RunLog, RunStatus, Trigger, TriggerType, WatchEvent};
use crate::domain::queues::Queue;
use crate::domain::sync::{
    IntentPayload, MessageKind, MissionImpact, StatusPayload, WorkspaceStatus,
};
use chrono::{Duration, TimeZone, Utc};
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
        effort: None,
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
        effort: None,
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
        effort: None,
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
        archived: false,
        paused_by_reconciliation: false,
        infra_node_id: None,
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
        auto_continue_at: None,
        auto_continue_action: None,
        active_run_queue_id: None,
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
        spec_committed_head: None,
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
    db.insert_scheduled_send("ss-x", "ping", "sess-x", None, Utc::now(), None)
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
        .unwrap()
        .0;

    let search = db
        .search_intelligence_nodes("connection", Some("pattern"), 10)
        .unwrap();
    assert_eq!(search.results.len(), 1);
    assert_eq!(search.results[0].id, "node-b");

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

    // 1. Both terms match the same node — still found (OR semantics).
    let both = db
        .search_intelligence_nodes("alpha beta", None, 10)
        .unwrap();
    assert_eq!(both.results.len(), 1);
    assert_eq!(both.results[0].id, "node-multi");

    // 2. One present term + one absent term now returns the partial match (OR).
    let partial = db.search_intelligence_nodes("alpha zzz", None, 10).unwrap();
    assert_eq!(partial.results.len(), 1);
    assert_eq!(partial.results[0].id, "node-multi");
    assert_eq!(partial.examined_count, 1);

    // 3. Single-term queries keep working as before.
    let single = db.search_intelligence_nodes("beta", None, 10).unwrap();
    assert_eq!(single.results.len(), 1);
    assert_eq!(single.results[0].id, "node-multi");

    // 4. Empty/whitespace-only queries return empty results but report count.
    let empty = db.search_intelligence_nodes("   ", None, 10).unwrap();
    assert!(empty.results.is_empty());
    assert_eq!(empty.examined_count, 1);
}

#[test]
fn test_intelligence_search_or_ranking() {
    let db = test_db();

    // Node A: matches "retry" in body only → 1pt
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("node-a".to_string()),
        kind: "fact".to_string(),
        title: "Unrelated title".to_string(),
        body: "Discusses retry strategies.".to_string(),
        metadata: None,
        project_hash: Some("proj".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    // Node B: matches "retry" in title + "backoff" in body → 2 terms
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("node-b".to_string()),
        kind: "fact".to_string(),
        title: "Retry patterns".to_string(),
        body: "Covers backoff strategies.".to_string(),
        metadata: None,
        project_hash: Some("proj".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    // Node C: matches "retry", "backoff" and "future" in body → 3 terms
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("node-c".to_string()),
        kind: "fact".to_string(),
        title: "Overview".to_string(),
        body: "Retry logic, backoff, and future plans.".to_string(),
        metadata: None,
        project_hash: Some("proj".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    // Node D: matches nothing
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("node-d".to_string()),
        kind: "fact".to_string(),
        title: "Other".to_string(),
        body: "Nothing relevant.".to_string(),
        metadata: None,
        project_hash: Some("proj".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    // Seven-word query whose matching terms are spread across the nodes; the
    // four trailing words match nothing and must degrade rank without emptying.
    let result = db
        .search_intelligence_nodes(
            "retry backoff future missingp missingq missingr missings",
            None,
            10,
        )
        .unwrap();

    // Primary ordering is number of matching terms (guideline), so the
    // three-term body match outranks the two-term match that includes a title:
    //   node-c: 3 terms (retry+backoff+future, all body)
    //   node-b: 2 terms (retry in title, backoff in body)
    //   node-a: 1 term  (retry in body)
    //   node-d: excluded
    assert_eq!(result.results.len(), 3);
    assert_eq!(result.results[0].id, "node-c");
    assert_eq!(result.results[1].id, "node-b");
    assert_eq!(result.results[2].id, "node-a");
    assert_eq!(result.examined_count, 4);
}

#[test]
fn test_intelligence_search_ranks_title_over_body_on_equal_term_count() {
    let db = test_db();

    // Both nodes match exactly one term; the tiebreak is *where* it matches.
    // The title match is inserted first so that a naive recency-only ordering
    // would put the body match on top — only the field weighting flips them.
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("title-hit".to_string()),
        kind: "fact".to_string(),
        title: "Resilience".to_string(),
        body: "Body text with no query terms at all.".to_string(),
        metadata: None,
        project_hash: Some("proj".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();
    db.upsert_intelligence_node(IntelligenceNodeInput {
        id: Some("body-hit".to_string()),
        kind: "fact".to_string(),
        title: "Generic heading".to_string(),
        body: "A passing mention of resilience somewhere in here.".to_string(),
        metadata: None,
        project_hash: Some("proj".to_string()),
        session_id: None,
        relations: None,
    })
    .unwrap();

    let result = db
        .search_intelligence_nodes("resilience", None, 10)
        .unwrap();
    assert_eq!(result.results.len(), 2);
    assert_eq!(result.results[0].id, "title-hit");
    assert_eq!(result.results[1].id, "body-hit");
}

#[test]
fn test_intelligence_search_zero_match_returns_count() {
    let db = test_db();

    for i in 0..3 {
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some(format!("node-{i}")),
            kind: "fact".to_string(),
            title: format!("Fact {i}"),
            body: format!("Body {i}"),
            metadata: None,
            project_hash: Some("proj".to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();
    }

    let result = db
        .search_intelligence_nodes("xyzzy nonexistent", None, 10)
        .unwrap();
    assert_eq!(result.results.len(), 0);
    assert_eq!(result.examined_count, 3);
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
        spec_committed_head: None,
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
        spec_committed_head: None,
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
    db.update_loop_edge_condition(&edge.id, &LoopEdgeCondition::Fail)
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
    let data_dir = tempdir().unwrap();
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

    let reconciled = db.reconcile_orphaned_loops(data_dir.path()).unwrap();
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

    // Test 2 (B18): the spec is marked `interrupted` in the same pass — not
    // reset to `pending`, since the run was cut short by something external,
    // not a failure of the work. Its completed work is preserved by the
    // worktree/commits, not by its status, and leaving it `running` would
    // make it invisible to queue selection (`queue_next_pending_spec_id`
    // picks `pending` and `interrupted` alike).
    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Interrupted);
    assert_eq!(spec_after.started_at, None);
    assert_eq!(spec_after.spec_start_head, None);
}

/// B18: the real incident — a queue-driven run's in-flight member (a
/// standalone spec, `loop_id: None`, never bound to the loop that's
/// currently running it) must be marked `interrupted` exactly like a
/// loop-bound spec is. Left `running`, it would be invisible to
/// `queue_next_pending_spec_id` forever — the orphan this whole fix exists
/// to prevent — and `queue_next_pending_spec_id` must pick an `interrupted`
/// member right back up, in the same position, exactly as it would a
/// `pending` one.
#[test]
fn reconcile_orphaned_loops_marks_queue_member_spec_interrupted() {
    let db = test_db();
    let data_dir = tempdir().unwrap();
    let mut lp = sample_loop("wf-orphan-queue");
    lp.status = LoopStatus::Running;
    lp.active_run_queue_id = Some("queue-1".to_string());
    db.insert_loop(&lp).unwrap();

    let mut spec = sample_loop_spec("unused-loop-id", "spec-orphan-queue", 1);
    spec.loop_id = None; // queue membership never binds the spec to a loop
    spec.status = LoopSpecStatus::Running;
    db.insert_loop_spec(&spec).unwrap();
    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &spec.id, None).unwrap();

    let node = sample_loop_node(&spec.id, "node-orphan-queue", 1);
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&LoopNodeRun {
        id: "run-orphan-queue".to_string(),
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

    assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 1);

    let lp_after = db.get_loop(&lp.id).unwrap().unwrap();
    assert_eq!(lp_after.status, LoopStatus::Paused);
    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Interrupted);
    // The queue's live pick can now find it again, exactly as it would a
    // `pending` member.
    assert_eq!(
        db.queue_next_pending_spec_id("queue-1").unwrap().as_deref(),
        Some(spec.id.as_str())
    );
}

/// R3 (B18): the defensive selection safety net. A queue member left
/// `running` with no `loop_runs` row proving it's still live in *this*
/// daemon's lifetime must be flagged as stale — but a member whose `running`
/// node run really does carry the current boot id (i.e. genuinely still in
/// flight right now) must not be.
#[test]
fn queue_stale_running_members_flags_only_the_member_with_no_live_run() {
    let db = test_db();
    let lp = sample_loop("wf-queue-stale");
    db.insert_loop(&lp).unwrap();

    let mut stale = sample_loop_spec("unused-loop-id", "spec-stale", 1);
    stale.loop_id = None;
    stale.status = LoopSpecStatus::Running;
    let mut live = sample_loop_spec("unused-loop-id", "spec-live", 2);
    live.loop_id = None;
    live.status = LoopSpecStatus::Running;
    db.insert_loop_spec(&stale).unwrap();
    db.insert_loop_spec(&live).unwrap();

    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &stale.id, None).unwrap();
    db.append_queue_member("queue-1", &live.id, None).unwrap();

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
        .queue_stale_running_members("queue-1", Some("boot-current"))
        .unwrap();
    assert_eq!(stale_members, vec![stale.id]);
}

#[test]
fn reconcile_orphaned_loops_is_idempotent() {
    let db = test_db();
    let data_dir = tempdir().unwrap();
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

    let first_pass = db.reconcile_orphaned_loops(data_dir.path()).unwrap();
    assert_eq!(first_pass, 1);
    let lp_after_first = db.get_loop(&lp.id).unwrap().unwrap();
    let run_after_first = db.get_loop_run(&run.id).unwrap().unwrap();

    // Test 3: a second reconcile pass finds nothing left to reconcile, and
    // leaves the already-paused loop/run untouched.
    let second_pass = db.reconcile_orphaned_loops(data_dir.path()).unwrap();
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
    let data_dir = tempdir().unwrap();
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

    assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 1);

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
    let data_dir = tempdir().unwrap();
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
    assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 1);
    let run_after = db.get_loop_run(&run.id).unwrap().unwrap();
    assert_ne!(run_after.status, LoopRunStatus::Running);
}

#[test]
fn reconcile_orphaned_loops_leaves_completed_loop_untouched() {
    let db = test_db();
    let data_dir = tempdir().unwrap();
    let mut lp = sample_loop("wf-completed");
    lp.status = LoopStatus::Completed;
    db.insert_loop(&lp).unwrap();

    // Test 4: a `Completed` loop is not reconciled.
    let reconciled = db.reconcile_orphaned_loops(data_dir.path()).unwrap();
    assert_eq!(reconciled, 0);
    let lp_after = db.get_loop(&lp.id).unwrap().unwrap();
    assert_eq!(lp_after.status, LoopStatus::Completed);
}

/// The ownership gate (second line of defence, alongside never calling this
/// from `run_stdio_server` at all — see the doc comment on
/// `reconcile_orphaned_loops`): when the on-disk `daemon.pid` names a *live*
/// process that isn't this one, some other process owns the daemon
/// lifecycle right now, and this call must not touch graph state at all —
/// no signal, no pause, no interrupted-run marking.
#[tokio::test]
async fn reconcile_orphaned_loops_skips_everything_when_foreign_daemon_pid_is_live() {
    let Some(current_boot_id) = crate::system::boot_id() else {
        return;
    };

    let data_dir = tempdir().unwrap();
    // Stand in for "a live daemon other than us": a real, still-running
    // process whose pid is guaranteed not to equal this test process's own.
    let mut foreign_daemon = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn stand-in foreign daemon process");
    std::fs::write(
        data_dir.path().join("daemon.pid"),
        foreign_daemon.id().to_string(),
    )
    .unwrap();

    let db = test_db();
    let mut lp = sample_loop("wf-foreign-daemon-owned");
    lp.status = LoopStatus::Running;
    let mut spec = sample_loop_spec(&lp.id, "spec-foreign-daemon-owned", 1);
    spec.status = LoopSpecStatus::Running;
    let node = sample_loop_node(&spec.id, "node-foreign-daemon-owned", 1);
    // A pid/boot_id that WOULD be killed if the gate failed to short-circuit
    // (same boot, matching the same-boot kill precondition tested elsewhere).
    let run = LoopNodeRun {
        id: "run-foreign-daemon-owned".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: Some(foreign_daemon.id() as i64),
        boot_id: Some(current_boot_id),
        session_id: None,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();

    let reconciled = db.reconcile_orphaned_loops(data_dir.path()).unwrap();
    assert_eq!(
        reconciled, 0,
        "must not report any loop reconciled while a foreign live daemon owns the pid file"
    );

    let lp_after = db.get_loop(&lp.id).unwrap().unwrap();
    assert_eq!(
        lp_after.status,
        LoopStatus::Running,
        "the graph must be left exactly as found, not paused"
    );
    let run_after = db.get_loop_run(&run.id).unwrap().unwrap();
    assert_eq!(
        run_after.status,
        LoopRunStatus::Running,
        "the run must not be reported as interrupted by a restart that never happened"
    );

    // The pid named in the "foreign daemon" run must never have been
    // signaled — prove it's still alive rather than merely unreaped.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        foreign_daemon.try_wait().unwrap(),
        None,
        "the gate must never signal the pid it protects"
    );
    let _ = foreign_daemon.kill();
    let _ = foreign_daemon.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn reconcile_orphaned_loops_skips_kill_for_own_ancestor_pid() {
    let Some(current_boot_id) = crate::system::boot_id() else {
        return;
    };

    // Read our own PPid — the parent of this test process. This process's
    // own pid can't be used (it would be killed), but the parent is an
    // ancestor and must be skipped by the ancestor guard.
    let ppid: u32 = std::fs::read_to_string("/proc/self/status")
        .expect("read /proc/self/status")
        .lines()
        .find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|rest| rest.trim().parse::<u32>().ok())
        })
        .expect("PPid line must be present");
    assert!(ppid != 0 && ppid != 1);
    // Sanity: parent must be alive at test start.
    assert!(
        crate::daemon::process::is_process_running(ppid),
        "parent pid {ppid} must be alive"
    );

    let db = test_db();
    let data_dir = tempdir().unwrap();
    let mut lp = sample_loop("wf-ancestor-skip");
    lp.status = LoopStatus::Running;
    let mut spec = sample_loop_spec(&lp.id, "spec-ancestor-skip", 1);
    spec.status = LoopSpecStatus::Running;
    let node = sample_loop_node(&spec.id, "node-ancestor-skip", 1);
    let run = LoopNodeRun {
        id: "run-ancestor-skip".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: Some(ppid as i64),
        boot_id: Some(current_boot_id),
        session_id: None,
    };

    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();

    let reconciled = db.reconcile_orphaned_loops(data_dir.path()).unwrap();
    assert_eq!(reconciled, 1);

    // Run must be marked interrupted in DB, but parent process must still be alive.
    let run_after = db.get_loop_run(&run.id).unwrap().unwrap();
    assert_ne!(run_after.status, LoopRunStatus::Running);
    assert_eq!(
        run_after
            .output
            .as_ref()
            .and_then(|value| value.get("interrupted")),
        Some(&serde_json::json!(true))
    );
    assert!(
        crate::daemon::process::is_process_running(ppid),
        "ancestor pid {ppid} must still be alive — the guard must have skipped the kill"
    );
}

#[test]
fn new_safe_skips_migration_when_foreign_daemon_is_live() {
    let data_dir = tempdir().unwrap();
    // Simulate a live daemon by writing our own pid (guaranteed live).
    std::fs::write(
        data_dir.path().join("daemon.pid"),
        std::process::id().to_string(),
    )
    .unwrap();

    let db_path = data_dir.path().join("fresh.db");
    // Empty DB, but foreign daemon is live — new_safe must skip migrations.
    let _db = Database::new_safe(&db_path, data_dir.path())
        .expect("new_safe should open without migrating");
    drop(_db);
    // Verify no tables were created: `loops` table must not exist.
    let loops_exists: i32 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'loops'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        loops_exists, 0,
        "new_safe with a live daemon must not have run migrations (loops table must not exist)"
    );

    // Without the foreign daemon, the same path must migrate normally.
    std::fs::remove_file(data_dir.path().join("daemon.pid")).unwrap();
    let _db2 = Database::new_safe(&db_path, data_dir.path())
        .expect("new_safe without daemon should migrate");
    drop(_db2);
    let loops_exists_after: i32 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'loops'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        loops_exists_after, 1,
        "new_safe without a live daemon must run migrations"
    );
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

fn sample_queue(id: &str) -> Queue {
    Queue {
        id: id.to_string(),
        name: format!("{id} name"),
        created_at: Utc::now(),
    }
}

#[test]
fn queue_and_members_round_trip_through_insert_and_get_details() {
    let db = test_db();
    for id in ["spec-a", "spec-b", "spec-c"] {
        db.insert_loop_spec(&sample_standalone_spec(id, None))
            .unwrap();
    }
    db.insert_queue(&sample_queue("queue-1")).unwrap();

    db.append_queue_member("queue-1", "spec-a", None).unwrap();
    db.append_queue_member("queue-1", "spec-b", None).unwrap();
    db.append_queue_member("queue-1", "spec-c", None).unwrap();
    assert_eq!(
        db.list_queue_member_spec_ids("queue-1").unwrap(),
        vec!["spec-a", "spec-b", "spec-c"]
    );
    assert!(db.queue_has_member("queue-1", "spec-b").unwrap());

    let details = db.get_queue_details("queue-1").unwrap().unwrap();
    assert_eq!(details.queue.name, "queue-1 name");
    assert_eq!(
        details
            .members
            .iter()
            .map(|spec| spec.id.clone())
            .collect::<Vec<_>>(),
        vec!["spec-a", "spec-b", "spec-c"]
    );

    assert!(db.remove_queue_member("queue-1", "spec-b").unwrap());
    assert!(!db.queue_has_member("queue-1", "spec-b").unwrap());
    assert_eq!(
        db.list_queue_member_spec_ids("queue-1").unwrap(),
        vec!["spec-a", "spec-c"]
    );

    let names = db
        .list_queues()
        .unwrap()
        .into_iter()
        .map(|queue| queue.id)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["queue-1"]);
    assert!(db.get_queue("does-not-exist").unwrap().is_none());
}

#[test]
fn reorder_queue_members_replaces_positions_in_given_order() {
    let db = test_db();
    for id in ["spec-a", "spec-b", "spec-c"] {
        db.insert_loop_spec(&sample_standalone_spec(id, None))
            .unwrap();
    }
    db.insert_queue(&sample_queue("queue-1")).unwrap();
    for id in ["spec-a", "spec-b", "spec-c"] {
        db.append_queue_member("queue-1", id, None).unwrap();
    }

    let order = vec![
        "spec-c".to_string(),
        "spec-a".to_string(),
        "spec-b".to_string(),
    ];
    db.reorder_queue_members("queue-1", &order).unwrap();

    assert_eq!(db.list_queue_member_spec_ids("queue-1").unwrap(), order);
}

#[test]
fn append_queue_member_rejects_nonexistent_spec() {
    let db = test_db();
    db.insert_queue(&sample_queue("queue-1")).unwrap();

    let error = db
        .append_queue_member("queue-1", "ghost-spec", None)
        .unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("foreign key"),
        "{error}"
    );
}

#[test]
fn deleting_a_spec_cascades_its_queue_membership() {
    let db = test_db();
    db.insert_loop_spec(&sample_standalone_spec("spec-a", None))
        .unwrap();
    db.insert_queue(&sample_queue("queue-1")).unwrap();
    db.append_queue_member("queue-1", "spec-a", None).unwrap();

    db.delete_loop_spec("spec-a").unwrap();

    assert!(db.list_queue_member_spec_ids("queue-1").unwrap().is_empty());
}

// ── RS3: context groups within a queue ──────────────────────────

/// Seed a `loop_runs` row for `spec_id`/`node_id` carrying `session_id`, then
/// stamp `spec_id`'s terminal status — the shape a completed/failed grouped
/// sibling leaves behind for `group_session_for_node` to read.
fn seed_group_sibling(
    db: &Database,
    loop_id: &str,
    spec_id: &str,
    node_id: &str,
    session_id: Option<&str>,
    status: LoopSpecStatus,
) {
    let run = LoopNodeRun {
        id: format!("run-{spec_id}-{node_id}"),
        loop_id: loop_id.to_string(),
        spec_id: spec_id.to_string(),
        node_id: node_id.to_string(),
        status: LoopRunStatus::Pass,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: Some(Utc::now()),
        iteration: 1,
        pid: None,
        boot_id: None,
        session_id: session_id.map(str::to_string),
    };
    db.insert_loop_run(&run).unwrap();
    db.update_loop_spec_status(spec_id, status, Some(Utc::now()), Some(Utc::now()))
        .unwrap();
}

/// Scaffold a loop with two grouped members (`spec-a`, `spec-b`) and one
/// grouped/ungrouped setup, returning the db. Both share a single loop-level
/// node id `node-impl`.
fn rs3_fixture() -> Database {
    let db = test_db();
    let lp = sample_loop("wf-rs3");
    db.insert_loop(&lp).unwrap();
    for id in ["spec-a", "spec-b", "spec-c"] {
        db.insert_loop_spec(&sample_standalone_spec(id, None))
            .unwrap();
    }
    // A loop-level node the runs can reference (loop_runs FK to loop_nodes).
    let mut node = sample_loop_node("spec-a", "node-impl", 1);
    node.spec_id = None;
    node.loop_id = Some(lp.id.clone());
    db.insert_loop_node(&node).unwrap();
    let mut review = sample_loop_node("spec-a", "node-review", 2);
    review.spec_id = None;
    review.loop_id = Some(lp.id);
    db.insert_loop_node(&review).unwrap();
    db.insert_queue(&sample_queue("queue-1")).unwrap();
    db
}

#[test]
fn group_name_persists_through_add_and_reorder() {
    let db = rs3_fixture();
    db.append_queue_member("queue-1", "spec-a", Some("ctx"))
        .unwrap();
    db.append_queue_member("queue-1", "spec-b", Some("ctx"))
        .unwrap();
    db.append_queue_member("queue-1", "spec-c", None).unwrap();

    // Groups are readable per member and in list order.
    assert_eq!(
        db.queue_member_group("queue-1", "spec-a")
            .unwrap()
            .as_deref(),
        Some("ctx")
    );
    assert_eq!(db.queue_member_group("queue-1", "spec-c").unwrap(), None);
    assert_eq!(
        db.list_queue_member_groups("queue-1").unwrap(),
        vec![
            ("spec-a".to_string(), Some("ctx".to_string())),
            ("spec-b".to_string(), Some("ctx".to_string())),
            ("spec-c".to_string(), None),
        ]
    );

    // Reorder must move rows AND preserve each row's group_name.
    db.reorder_queue_members(
        "queue-1",
        &[
            "spec-c".to_string(),
            "spec-a".to_string(),
            "spec-b".to_string(),
        ],
    )
    .unwrap();
    assert_eq!(
        db.list_queue_member_groups("queue-1").unwrap(),
        vec![
            ("spec-c".to_string(), None),
            ("spec-a".to_string(), Some("ctx".to_string())),
            ("spec-b".to_string(), Some("ctx".to_string())),
        ]
    );
}

#[test]
fn group_session_for_node_returns_completed_siblings_session() {
    let db = rs3_fixture();
    db.append_queue_member("queue-1", "spec-a", Some("ctx"))
        .unwrap();
    db.append_queue_member("queue-1", "spec-b", Some("ctx"))
        .unwrap();
    seed_group_sibling(
        &db,
        "wf-rs3",
        "spec-a",
        "node-impl",
        Some("ses-a"),
        LoopSpecStatus::Completed,
    );

    // spec-b's first visit to node-impl inherits spec-a's warm session.
    assert_eq!(
        db.group_session_for_node("queue-1", "ctx", "spec-b", "node-impl")
            .unwrap()
            .as_deref(),
        Some("ses-a")
    );
    // A node the sibling never ran → no session → cold start.
    assert_eq!(
        db.group_session_for_node("queue-1", "ctx", "spec-b", "node-review")
            .unwrap(),
        None
    );
}

#[test]
fn group_session_taint_on_failed_nearest_sibling() {
    let db = rs3_fixture();
    // Three grouped siblings: a completed, then a failed, then the current one.
    db.append_queue_member("queue-1", "spec-a", Some("ctx"))
        .unwrap();
    db.append_queue_member("queue-1", "spec-b", Some("ctx"))
        .unwrap();
    db.append_queue_member("queue-1", "spec-c", Some("ctx"))
        .unwrap();
    seed_group_sibling(
        &db,
        "wf-rs3",
        "spec-a",
        "node-impl",
        Some("ses-a"),
        LoopSpecStatus::Completed,
    );
    // The nearest sibling to spec-c FAILED (even if it captured a session).
    seed_group_sibling(
        &db,
        "wf-rs3",
        "spec-b",
        "node-impl",
        Some("ses-b"),
        LoopSpecStatus::Failed,
    );

    // Taint: the broken chain forces a cold start — the earlier completed
    // spec-a session is NOT resurrected across the failure.
    assert_eq!(
        db.group_session_for_node("queue-1", "ctx", "spec-c", "node-impl")
            .unwrap(),
        None
    );
}

#[test]
fn group_session_is_independent_per_node() {
    let db = rs3_fixture();
    db.append_queue_member("queue-1", "spec-a", Some("ctx"))
        .unwrap();
    db.append_queue_member("queue-1", "spec-b", Some("ctx"))
        .unwrap();
    // spec-a completed after capturing a DISTINCT session on each node.
    let run_impl = LoopNodeRun {
        id: "run-a-impl".to_string(),
        loop_id: "wf-rs3".to_string(),
        spec_id: "spec-a".to_string(),
        node_id: "node-impl".to_string(),
        status: LoopRunStatus::Pass,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: Some(Utc::now()),
        iteration: 1,
        pid: None,
        boot_id: None,
        session_id: Some("ses-impl".to_string()),
    };
    db.insert_loop_run(&run_impl).unwrap();
    let run_review = LoopNodeRun {
        id: "run-a-review".to_string(),
        node_id: "node-review".to_string(),
        session_id: Some("ses-review".to_string()),
        ..run_impl
    };
    db.insert_loop_run(&run_review).unwrap();
    db.update_loop_spec_status(
        "spec-a",
        LoopSpecStatus::Completed,
        Some(Utc::now()),
        Some(Utc::now()),
    )
    .unwrap();

    // Implementer and reviewer sessions stay independent.
    assert_eq!(
        db.group_session_for_node("queue-1", "ctx", "spec-b", "node-impl")
            .unwrap()
            .as_deref(),
        Some("ses-impl")
    );
    assert_eq!(
        db.group_session_for_node("queue-1", "ctx", "spec-b", "node-review")
            .unwrap()
            .as_deref(),
        Some("ses-review")
    );
}

#[test]
fn ungrouped_and_cross_group_members_never_cross_resume() {
    let db = rs3_fixture();
    // spec-a grouped "ctx", spec-b ungrouped, spec-c in a DIFFERENT group.
    db.append_queue_member("queue-1", "spec-a", Some("ctx"))
        .unwrap();
    db.append_queue_member("queue-1", "spec-b", None).unwrap();
    db.append_queue_member("queue-1", "spec-c", Some("other"))
        .unwrap();
    seed_group_sibling(
        &db,
        "wf-rs3",
        "spec-a",
        "node-impl",
        Some("ses-a"),
        LoopSpecStatus::Completed,
    );

    // An ungrouped member never inherits (queried with its own — absent — group).
    assert_eq!(db.queue_member_group("queue-1", "spec-b").unwrap(), None);
    // A member in another group does not see "ctx"'s session.
    assert_eq!(
        db.group_session_for_node("queue-1", "other", "spec-c", "node-impl")
            .unwrap(),
        None
    );
}

#[test]
fn group_name_column_is_added_to_a_pre_rs3_queue_members_table() {
    // Simulate a pre-RS3 database whose `queue_members` predates `group_name`.
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    {
        let conn = rusqlite::Connection::open(&path).expect("open raw db");
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
                spec_queue TEXT
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
             CREATE TABLE queues (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             -- Pre-RS3 queue_members: no group_name column.
             CREATE TABLE queue_members (
                queue_id TEXT NOT NULL REFERENCES queues(id) ON DELETE CASCADE,
                spec_id TEXT NOT NULL REFERENCES loop_specs(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                PRIMARY KEY (queue_id, spec_id)
             );
             INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES ('legacy-spec', NULL, 'Spec', 1, 'pending');
             INSERT INTO queues (id, name, created_at) VALUES ('queue-1', 'Queue', 0);
             INSERT INTO queue_members (queue_id, spec_id, position)
                 VALUES ('queue-1', 'legacy-spec', 1);",
        )
        .expect("seed pre-RS3 schema");
    }

    // Migration adds the nullable column; the pre-existing row reads back as
    // ungrouped (NULL), and re-running init stays safe (idempotent guard).
    let db = Database::new(&path).expect("open pre-RS3 db, running migration");
    assert_eq!(
        db.queue_member_group("queue-1", "legacy-spec").unwrap(),
        None
    );
    drop(db);
    let db = Database::new(&path).expect("re-open is idempotent");
    assert_eq!(
        db.queue_member_group("queue-1", "legacy-spec").unwrap(),
        None
    );
}

#[test]
fn queues_migration_is_idempotent_and_a_pre_r4_database_opens_cleanly() {
    // Simulate a pre-R4 database: `loops` has the old `spec_queue` column
    // (7f2efdf) but no `queues`/`queue_members` tables at all.
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
                spec_queue TEXT
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
             INSERT INTO loops (id, name, workdir, status, created_at, spec_queue)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'draft', 0, NULL);
             INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES ('legacy-spec', 'legacy-loop', 'Spec', 1, 'pending');",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must succeed and add
    // the queues tables without disturbing existing rows.
    let db = Database::new(&path).expect("open pre-R4 db, running migration");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.name, "Legacy");
    db.insert_queue(&sample_queue("queue-1")).unwrap();
    db.append_queue_member("queue-1", "legacy-spec", None)
        .unwrap();
    assert_eq!(
        db.list_queue_member_spec_ids("queue-1").unwrap(),
        vec!["legacy-spec"]
    );
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent).
    let db = Database::new(&path).expect("reopen db after migration already applied");
    assert_eq!(
        db.list_queue_member_spec_ids("queue-1").unwrap(),
        vec!["legacy-spec"]
    );

    // The retired `spec_queue` column is never written by current code: a
    // freshly inserted loop leaves it NULL.
    db.insert_loop(&sample_loop("fresh-loop")).unwrap();
    let raw: Option<String> = db
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT spec_queue FROM loops WHERE id = 'fresh-loop'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, None);
}

#[test]
fn active_run_queue_id_migration_is_idempotent_and_a_pre_b8_database_opens_cleanly() {
    // Simulate a pre-B8 database: `loops` has `autorun_at` and `queues`/
    // `queue_members` already exist, but `loops` predates `active_run_queue_id`.
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
                spec_queue TEXT
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
             CREATE TABLE queues (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE queue_members (
                queue_id TEXT NOT NULL REFERENCES queues(id) ON DELETE CASCADE,
                spec_id TEXT NOT NULL REFERENCES loop_specs(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                PRIMARY KEY (queue_id, spec_id)
             );
             INSERT INTO loops (id, name, workdir, status, created_at)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'failed', 0);
             INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES ('legacy-spec', NULL, 'Spec', 1, 'pending');
             INSERT INTO queues (id, name, created_at) VALUES ('queue-1', 'queue-1', 0);
             INSERT INTO queue_members (queue_id, spec_id, position)
                 VALUES ('queue-1', 'legacy-spec', 1);",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must succeed and add
    // `active_run_queue_id` without disturbing existing rows.
    let db = Database::new(&path).expect("open pre-B8 db, running migration");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.name, "Legacy");
    assert_eq!(lp.active_run_queue_id, None);

    // The new column is actually usable: persist a run context and reset
    // through the shared path picks up the queue's members.
    db.set_loop_active_run_queue("legacy-loop", Some("queue-1"))
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
    assert_eq!(lp.active_run_queue_id.as_deref(), Some("queue-1"));
}

// RETIRED-SCHEMA-NAME-BEGIN (see `no_retired_schema_name_identifiers_remain_outside_its_migration`)
#[test]
fn legacy_queue_table_names_migrate_preserving_member_order_and_context_groups() {
    // Simulate a database still on the previous-generation queue schema (the
    // exact shape every pre-existing installation has on disk today, with
    // its own table, column, and index names — see the raw SQL below). Two
    // queues, several members each, deliberately inserted out of position
    // order, spanning multiple (and no) context groups — the thing most
    // worth not losing is the position-driven order and each member's group.
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
                spec_pool TEXT,
                active_run_pool_id TEXT,
                on_completed TEXT
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
                group_name TEXT,
                PRIMARY KEY (pool_id, spec_id)
             );
             CREATE UNIQUE INDEX idx_pool_members_position
                 ON pool_members(pool_id, position);
             INSERT INTO loops (id, name, workdir, status, created_at, active_run_pool_id)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'paused', 0, 'queue-a');
             INSERT INTO pools (id, name, created_at) VALUES
                 ('queue-a', 'Queue A', 0),
                 ('queue-b', 'Queue B', 0);",
        )
        .expect("seed legacy schema");

        for (spec_id, position) in [
            ("spec-a1", 1),
            ("spec-a2", 2),
            ("spec-a3", 3),
            ("spec-a4", 4),
            ("spec-b1", 1),
            ("spec-b2", 2),
        ] {
            conn.execute(
                "INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES (?1, NULL, ?1, 1, 'pending')",
                [spec_id],
            )
            .unwrap();
            let _ = position;
        }

        // Insert members out of position order to prove the migration
        // preserves `position`, not insertion order.
        conn.execute(
            "INSERT INTO pool_members (pool_id, spec_id, position, group_name) VALUES
                ('queue-a', 'spec-a3', 3, NULL),
                ('queue-a', 'spec-a1', 1, 'group-1'),
                ('queue-a', 'spec-a4', 4, 'group-2'),
                ('queue-a', 'spec-a2', 2, 'group-1'),
                ('queue-b', 'spec-b2', 2, 'group-3'),
                ('queue-b', 'spec-b1', 1, NULL)",
            [],
        )
        .unwrap();
    }

    let db = Database::new(&path).expect("open legacy-schema db, running the rename migration");

    // Table/index/column identity: old names are gone, new ones hold the data.
    let conn = db.conn.lock().unwrap();
    let old_objects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('pools', 'pool_members', 'idx_pool_members_position')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_objects, 0, "legacy-named objects must not survive");
    let new_objects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('queues', 'queue_members', 'idx_queue_members_position')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(new_objects, 3, "renamed queue objects must exist");
    drop(conn);

    // Data identity: members present, in the same order, in the same groups.
    assert_eq!(
        db.list_queue_member_groups("queue-a").unwrap(),
        vec![
            ("spec-a1".to_string(), Some("group-1".to_string())),
            ("spec-a2".to_string(), Some("group-1".to_string())),
            ("spec-a3".to_string(), None),
            ("spec-a4".to_string(), Some("group-2".to_string())),
        ]
    );
    assert_eq!(
        db.list_queue_member_groups("queue-b").unwrap(),
        vec![
            ("spec-b1".to_string(), None),
            ("spec-b2".to_string(), Some("group-3".to_string())),
        ]
    );

    // The loop's active-run reference survived under its renamed column.
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.active_run_queue_id.as_deref(), Some("queue-a"));
}

#[test]
fn legacy_queue_table_rename_is_a_noop_on_an_already_migrated_database() {
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    // First open: fresh database, already on the current (post-rename) schema.
    let db = Database::new(&path).expect("create fresh db");
    db.insert_loop_spec(&LoopSpec {
        id: "spec-1".to_string(),
        loop_id: None,
        name: "spec-1".to_string(),
        description: None,
        position: 1,
        parallelizable: false,
        status: LoopSpecStatus::Pending,
        started_at: None,
        completed_at: None,
        spec_start_head: None,
        spec_committed_head: None,
        workdir: None,
        completed_via: None,
        completed_via_reason: None,
        completed_via_at: None,
    })
    .unwrap();
    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "Queue".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", "spec-1", Some("group-1"))
        .unwrap();
    drop(db);

    // Reopening must not error and must not touch existing data — the
    // rename's guard (legacy table absent) makes it a pure no-op.
    let db = Database::new(&path).expect("reopen already-migrated db");
    assert_eq!(
        db.list_queue_member_groups("queue-1").unwrap(),
        vec![("spec-1".to_string(), Some("group-1".to_string()))]
    );
    let conn = db.conn.lock().unwrap();
    let legacy_table_present: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'pools'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        legacy_table_present, 0,
        "the migration must not resurrect the legacy table"
    );
}
// RETIRED-SCHEMA-NAME-END

#[test]
fn a_schema_version_newer_than_this_binary_supports_fails_loudly_instead_of_starting_empty() {
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    let db = Database::new(&path).expect("create fresh db");
    db.set_state("schema_version", "999").unwrap();
    drop(db);

    let result = Database::new(&path);
    let message = match result {
        Ok(_) => panic!(
            "a database stamped with a newer schema version than this binary knows must not open"
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        message.contains("999") && message.to_lowercase().contains("schema version"),
        "error must name the version mismatch: {message}"
    );
}

#[test]
fn auto_continue_migration_is_idempotent_and_a_pre_migration_database_opens_cleanly() {
    // Simulate a database written before `auto_continue_at`/`auto_continue_action`
    // existed: `loops` has every other current column, including `autorun_at`.
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
                spec_queue TEXT,
                active_run_queue_id TEXT,
                on_completed TEXT
             );
             INSERT INTO loops (id, name, workdir, status, created_at)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'paused', 0);",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must succeed and add
    // both new columns, NULL on the existing row.
    let db = Database::new(&path).expect("open pre-auto-continue db, running migration");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.name, "Legacy");
    assert_eq!(lp.auto_continue_at, None);
    assert_eq!(lp.auto_continue_action, None);

    // The new columns are actually usable after migration.
    let at = chrono::DateTime::from_timestamp(
        (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
        0,
    )
    .unwrap();
    db.schedule_loop_auto_continue("legacy-loop", at, Some("skip_next_spec"))
        .unwrap();
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent).
    let db = Database::new(&path).expect("reopen db after migration already applied");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.auto_continue_at, Some(at));
    assert_eq!(lp.auto_continue_action.as_deref(), Some("skip_next_spec"));
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
        .get_operational_session(&format!("run:{run_id}"))
        .unwrap()
        .unwrap();
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
fn unbind_loop_spec_clears_loop_id() {
    let db = test_db();
    let lp = sample_loop("loop-unbind");
    db.insert_loop(&lp).unwrap();
    let spec = sample_loop_spec(&lp.id, "spec-unbind", 1);
    let name = spec.name.clone();
    let description = spec.description.clone();
    db.insert_loop_spec(&spec).unwrap();

    assert!(db.unbind_loop_spec(&spec.id).unwrap());

    let after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert!(after.loop_id.is_none());
    assert_eq!(after.name, name);
    assert_eq!(after.description, description);
}

#[test]
fn unbind_loop_spec_noop_for_standalone() {
    let db = test_db();
    let mut spec = sample_loop_spec("unused", "spec-standalone", 1);
    spec.loop_id = None;
    db.insert_loop_spec(&spec).unwrap();

    assert!(!db.unbind_loop_spec(&spec.id).unwrap());
}

#[test]
fn unbind_loop_spec_preserves_execution_history() {
    let db = test_db();
    let lp = sample_loop("loop-unbind-hist");
    db.insert_loop(&lp).unwrap();
    let spec = sample_loop_spec(&lp.id, "spec-unbind-hist", 1);
    db.insert_loop_spec(&spec).unwrap();
    let node = sample_loop_node(&spec.id, "node-unbind-hist", 1);
    db.insert_loop_node(&node).unwrap();
    let run = LoopNodeRun {
        id: "run-unbind-hist".to_string(),
        loop_id: lp.id,
        spec_id: spec.id.clone(),
        node_id: node.id,
        status: LoopRunStatus::Pass,
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

    assert!(db.unbind_loop_spec(&spec.id).unwrap());

    let runs = db.list_loop_runs_for_spec(&spec.id).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "run-unbind-hist");
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
fn set_spec_admin_status_propagates_to_queue_selection() {
    let db = test_db();
    let mut spec = sample_loop_spec("unused", "spec-queue-prop", 1);
    spec.loop_id = None;
    db.insert_loop_spec(&spec).unwrap();

    let queue = Queue {
        id: "queue-test".to_string(),
        name: "queue-test".to_string(),
        created_at: Utc::now(),
    };
    db.insert_queue(&queue).unwrap();
    db.append_queue_member("queue-test", &spec.id, None)
        .unwrap();

    let before = db.queue_next_pending_spec_id("queue-test").unwrap();
    assert_eq!(before.as_deref(), Some(spec.id.as_str()));

    let outcome = db
        .set_spec_admin_status(&spec.id, LoopSpecStatus::Completed, "reason")
        .unwrap();
    assert!(matches!(outcome, SpecAdminStatusOutcome::Success));

    let after = db.queue_next_pending_spec_id("queue-test").unwrap();
    assert_eq!(
        after, None,
        "queue should not select completed spec anymore"
    );
}

/// `Interrupted` must be exactly as selectable as `Pending` — a spec cut
/// short by an external event (daemon restart, crash) is not less runnable
/// than a spec that never started, and queue order between the two statuses
/// is preserved (position, not status, decides who goes first).
#[test]
fn queue_next_pending_spec_id_picks_interrupted_spec_in_position_order() {
    let db = test_db();
    let queue = Queue {
        id: "queue-interrupted".to_string(),
        name: "queue-interrupted".to_string(),
        created_at: Utc::now(),
    };
    db.insert_queue(&queue).unwrap();

    let mut ahead = sample_loop_spec("unused", "spec-ahead-completed", 1);
    ahead.loop_id = None;
    ahead.status = LoopSpecStatus::Completed;
    db.insert_loop_spec(&ahead).unwrap();
    db.append_queue_member("queue-interrupted", &ahead.id, None)
        .unwrap();

    let mut interrupted = sample_loop_spec("unused", "spec-interrupted", 2);
    interrupted.loop_id = None;
    interrupted.status = LoopSpecStatus::Interrupted;
    db.insert_loop_spec(&interrupted).unwrap();
    db.append_queue_member("queue-interrupted", &interrupted.id, None)
        .unwrap();

    let mut pending = sample_loop_spec("unused", "spec-pending-after", 3);
    pending.loop_id = None;
    pending.status = LoopSpecStatus::Pending;
    db.insert_loop_spec(&pending).unwrap();
    db.append_queue_member("queue-interrupted", &pending.id, None)
        .unwrap();

    // Position 2 (`interrupted`) is picked before position 3 (`pending`) —
    // selection follows queue order, not a preference between the two
    // equally-runnable statuses.
    assert_eq!(
        db.queue_next_pending_spec_id("queue-interrupted")
            .unwrap()
            .as_deref(),
        Some(interrupted.id.as_str())
    );
}

#[test]
fn queue_running_spec_id_returns_first_running_member() {
    let db = test_db();
    let lp = sample_loop("wf-queue-running");
    db.insert_loop(&lp).unwrap();

    let mut spec_a = sample_loop_spec("unused", "spec-a", 1);
    spec_a.loop_id = None;
    spec_a.status = LoopSpecStatus::Running;
    let mut spec_b = sample_loop_spec("unused", "spec-b", 2);
    spec_b.loop_id = None;
    spec_b.status = LoopSpecStatus::Pending;
    db.insert_loop_spec(&spec_a).unwrap();
    db.insert_loop_spec(&spec_b).unwrap();

    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &spec_a.id, None).unwrap();
    db.append_queue_member("queue-1", &spec_b.id, None).unwrap();

    assert_eq!(
        db.queue_running_spec_id("queue-1").unwrap().as_deref(),
        Some(spec_a.id.as_str())
    );
}

#[test]
fn queue_running_spec_id_returns_none_when_no_running_member() {
    let db = test_db();
    let lp = sample_loop("wf-queue-no-running");
    db.insert_loop(&lp).unwrap();

    let mut spec = sample_loop_spec("unused", "spec-p", 1);
    spec.loop_id = None;
    spec.status = LoopSpecStatus::Pending;
    db.insert_loop_spec(&spec).unwrap();

    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &spec.id, None).unwrap();

    assert_eq!(db.queue_running_spec_id("queue-1").unwrap(), None);
}

#[test]
fn reconcile_stranded_queue_specs_resets_running_spec_with_no_active_run() {
    let db = test_db();
    let mut lp = sample_loop("wf-stranded");
    lp.status = LoopStatus::Paused;
    lp.active_run_queue_id = Some("queue-1".to_string());
    db.insert_loop(&lp).unwrap();

    let mut spec = sample_loop_spec("unused", "spec-stranded", 1);
    spec.loop_id = None;
    spec.status = LoopSpecStatus::Running;
    db.insert_loop_spec(&spec).unwrap();

    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &spec.id, None).unwrap();

    assert_eq!(db.reconcile_stranded_queue_specs().unwrap(), 1);

    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Pending);
    assert!(spec_after.started_at.is_none());
    assert!(spec_after.spec_start_head.is_none());
}

#[test]
fn reconcile_stranded_queue_specs_preserves_spec_with_active_run_in_current_boot() {
    let db = test_db();
    let mut lp = sample_loop("wf-stranded-live");
    lp.status = LoopStatus::Paused;
    lp.active_run_queue_id = Some("queue-1".to_string());
    db.insert_loop(&lp).unwrap();

    let mut spec = sample_loop_spec("unused", "spec-live", 1);
    spec.loop_id = None;
    spec.status = LoopSpecStatus::Running;
    db.insert_loop_spec(&spec).unwrap();

    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &spec.id, None).unwrap();

    let node = sample_loop_node(&spec.id, "node-live", 1);
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&LoopNodeRun {
        id: "run-live".to_string(),
        loop_id: lp.id,
        spec_id: spec.id.clone(),
        node_id: node.id,
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: None,
        boot_id: Some(crate::system::boot_id().unwrap_or_default()),
        session_id: None,
    })
    .unwrap();

    assert_eq!(
        db.reconcile_stranded_queue_specs().unwrap(),
        0,
        "spec with active run in current boot must not be reset"
    );

    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Running);
}

#[test]
fn reconcile_stranded_queue_specs_is_idempotent() {
    let db = test_db();
    let mut lp = sample_loop("wf-stranded-idem");
    lp.status = LoopStatus::Paused;
    lp.active_run_queue_id = Some("queue-1".to_string());
    db.insert_loop(&lp).unwrap();

    let mut spec = sample_loop_spec("unused", "spec-idem", 1);
    spec.loop_id = None;
    spec.status = LoopSpecStatus::Running;
    db.insert_loop_spec(&spec).unwrap();

    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &spec.id, None).unwrap();

    assert_eq!(db.reconcile_stranded_queue_specs().unwrap(), 1);
    assert_eq!(
        db.reconcile_stranded_queue_specs().unwrap(),
        0,
        "second pass must find nothing to reset"
    );
}

// ── B36: interrupted-run marking (no git stash) ──────────────────────────

fn init_git_repo(path: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git command failed to run");
        assert!(status.success(), "git {:?} failed", args);
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "Test"]);
    run(&["config", "user.email", "test@example.com"]);
    std::fs::write(path.join("README.md"), "test").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
}

fn git_head(path: &std::path::Path) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .expect("git rev-parse failed to run");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_is_clean(path: &std::path::Path) -> bool {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .expect("git status failed to run");
    output.stdout.is_empty()
}

#[test]
fn reconcile_orphaned_loops_marks_interrupted_and_leaves_dirty_worktree_untouched() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    let head = git_head(dir.path());
    // Stand in for a truncated write left behind by the killed process.
    std::fs::write(dir.path().join("truncated.rs"), "fn broken(").unwrap();
    assert!(!git_is_clean(dir.path()));

    let db = test_db();
    let data_dir = tempdir().unwrap();
    let mut lp = sample_loop("wf-orphan-interrupted-dirty");
    lp.status = LoopStatus::Running;
    lp.workdir = dir.path().to_string_lossy().to_string();
    let mut spec = sample_loop_spec(&lp.id, "spec-orphan-interrupted-dirty", 1);
    spec.status = LoopSpecStatus::Running;
    spec.spec_start_head = Some(head);
    let node = sample_loop_node(&spec.id, "node-orphan-interrupted-dirty", 1);
    let run = LoopNodeRun {
        id: "run-orphan-interrupted-dirty".to_string(),
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

    assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 1);

    assert!(
        !git_is_clean(dir.path()),
        "the engine must never touch git — uncommitted changes stay exactly as the interrupted run left them"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("truncated.rs")).unwrap(),
        "fn broken(",
        "the partial write itself must be untouched, not just 'still dirty'"
    );

    let run_after = db.get_loop_run(&run.id).unwrap().unwrap();
    let output = run_after.output.as_ref().expect("output recorded");
    assert_eq!(output.get("interrupted"), Some(&serde_json::json!(true)));
    assert!(
        output.get("quarantine").is_none(),
        "there is no quarantine mechanism left to report"
    );

    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Interrupted);
}

#[test]
fn reconcile_orphaned_loops_marks_interrupted_for_clean_worktree_too() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    let head = git_head(dir.path());
    assert!(git_is_clean(dir.path()));

    let db = test_db();
    let data_dir = tempdir().unwrap();
    let mut lp = sample_loop("wf-orphan-interrupted-clean");
    lp.status = LoopStatus::Running;
    lp.workdir = dir.path().to_string_lossy().to_string();
    let mut spec = sample_loop_spec(&lp.id, "spec-orphan-interrupted-clean", 1);
    spec.status = LoopSpecStatus::Running;
    spec.spec_start_head = Some(head);
    let node = sample_loop_node(&spec.id, "node-orphan-interrupted-clean", 1);
    let run = LoopNodeRun {
        id: "run-orphan-interrupted-clean".to_string(),
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

    assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 1);

    assert!(git_is_clean(dir.path()), "still nothing to touch");

    let run_after = db.get_loop_run(&run.id).unwrap().unwrap();
    let output = run_after.output.as_ref().expect("output recorded");
    assert_eq!(output.get("interrupted"), Some(&serde_json::json!(true)));
    assert!(output.get("quarantine").is_none());

    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(
        spec_after.status,
        LoopSpecStatus::Interrupted,
        "a spec becomes interrupted regardless of whether the worktree happened to be dirty"
    );
}

/// The engine must not require git at all: a scratch workdir that was never
/// `git init`ed still gets the spec marked `Interrupted`, with whatever
/// partial work it holds left completely alone.
#[test]
fn reconcile_orphaned_loops_marks_interrupted_in_non_git_workdir() {
    let dir = tempdir().unwrap();
    // Deliberately no `init_git_repo` — this workdir is not a repository.
    std::fs::write(dir.path().join("truncated.rs"), "fn broken(").unwrap();

    let db = test_db();
    let data_dir = tempdir().unwrap();
    let mut lp = sample_loop("wf-orphan-interrupted-no-git");
    lp.status = LoopStatus::Running;
    lp.workdir = dir.path().to_string_lossy().to_string();
    let mut spec = sample_loop_spec(&lp.id, "spec-orphan-interrupted-no-git", 1);
    spec.status = LoopSpecStatus::Running;
    spec.spec_start_head = None;
    let node = sample_loop_node(&spec.id, "node-orphan-interrupted-no-git", 1);
    let run = LoopNodeRun {
        id: "run-orphan-interrupted-no-git".to_string(),
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

    assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 1);

    assert_eq!(
        std::fs::read_to_string(dir.path().join("truncated.rs")).unwrap(),
        "fn broken(",
        "partial work in a non-git workdir must be left exactly as found"
    );

    let run_after = db.get_loop_run(&run.id).unwrap().unwrap();
    let output = run_after.output.as_ref().expect("output recorded");
    assert_eq!(output.get("interrupted"), Some(&serde_json::json!(true)));
    assert!(output.get("quarantine").is_none());

    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Interrupted);
}

#[test]
fn reconcile_orphaned_loops_interrupted_marking_is_idempotent_across_two_passes() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    let head = git_head(dir.path());
    std::fs::write(dir.path().join("truncated.rs"), "fn broken(").unwrap();

    let db = test_db();
    let data_dir = tempdir().unwrap();
    let mut lp = sample_loop("wf-orphan-interrupted-idempotent");
    lp.status = LoopStatus::Running;
    lp.workdir = dir.path().to_string_lossy().to_string();
    let mut spec = sample_loop_spec(&lp.id, "spec-orphan-interrupted-idempotent", 1);
    spec.status = LoopSpecStatus::Running;
    spec.spec_start_head = Some(head);
    let node = sample_loop_node(&spec.id, "node-orphan-interrupted-idempotent", 1);
    let run = LoopNodeRun {
        id: "run-orphan-interrupted-idempotent".to_string(),
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

    assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 1);
    let spec_after_first = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after_first.status, LoopSpecStatus::Interrupted);

    // Second pass: the loop is already `Paused`, so it's not even a
    // candidate — nothing should change again, and the worktree stays
    // exactly as it was after the first pass.
    assert_eq!(db.reconcile_orphaned_loops(data_dir.path()).unwrap(), 0);
    let spec_after_second = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after_second.status, LoopSpecStatus::Interrupted);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("truncated.rs")).unwrap(),
        "fn broken(",
        "a second pass must not touch the worktree"
    );
}

#[test]
fn reconcile_stranded_queue_specs_marks_spec_interrupted_and_leaves_worktree_untouched() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    let head = git_head(dir.path());
    std::fs::write(dir.path().join("truncated.rs"), "fn broken(").unwrap();

    let db = test_db();
    let mut lp = sample_loop("wf-stranded-interrupted");
    lp.status = LoopStatus::Paused;
    lp.workdir = dir.path().to_string_lossy().to_string();
    lp.active_run_queue_id = Some("queue-1".to_string());
    db.insert_loop(&lp).unwrap();

    let mut spec = sample_loop_spec("unused", "spec-stranded-interrupted", 1);
    spec.loop_id = None;
    spec.status = LoopSpecStatus::Running;
    spec.spec_start_head = Some(head);
    db.insert_loop_spec(&spec).unwrap();

    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &spec.id, None).unwrap();

    let node = sample_loop_node(&spec.id, "node-stranded-interrupted", 1);
    db.insert_loop_node(&node).unwrap();
    // A `loop_runs` row left `running` by a daemon that died before a
    // graceful path (e.g. `loop_report_blocker`) could finalize it — its
    // boot id is stale, proving it from a recorded fact rather than content.
    db.insert_loop_run(&LoopNodeRun {
        id: "run-stranded-interrupted".to_string(),
        loop_id: lp.id,
        spec_id: spec.id.clone(),
        node_id: node.id,
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: None,
        boot_id: Some("some-other-boot-that-is-not-current".to_string()),
        session_id: None,
    })
    .unwrap();

    assert_eq!(db.reconcile_stranded_queue_specs().unwrap(), 1);

    assert!(
        !git_is_clean(dir.path()),
        "the engine must never touch git here either"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("truncated.rs")).unwrap(),
        "fn broken("
    );

    let run_after = db
        .get_loop_run("run-stranded-interrupted")
        .unwrap()
        .unwrap();
    assert_ne!(run_after.status, LoopRunStatus::Running);
    let output = run_after.output.as_ref().expect("output recorded");
    assert_eq!(output.get("interrupted"), Some(&serde_json::json!(true)));
    assert!(output.get("quarantine").is_none());

    // The key behaviour change (B36 → this spec): a genuinely interrupted
    // spec is marked `Interrupted`, not silently reset to `Pending` — it
    // used to be indistinguishable from a spec that never started.
    let spec_after = db.get_loop_spec(&spec.id).unwrap().unwrap();
    assert_eq!(spec_after.status, LoopSpecStatus::Interrupted);
}

#[test]
fn reconcile_stranded_queue_specs_leaves_worktree_untouched_for_healthy_run() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    let head = git_head(dir.path());
    std::fs::write(dir.path().join("in-progress.rs"), "fn still_writing(").unwrap();

    let db = test_db();
    let mut lp = sample_loop("wf-stranded-healthy");
    lp.status = LoopStatus::Paused;
    lp.workdir = dir.path().to_string_lossy().to_string();
    lp.active_run_queue_id = Some("queue-1".to_string());
    db.insert_loop(&lp).unwrap();

    let mut spec = sample_loop_spec("unused", "spec-stranded-healthy", 1);
    spec.loop_id = None;
    spec.status = LoopSpecStatus::Running;
    spec.spec_start_head = Some(head);
    db.insert_loop_spec(&spec).unwrap();

    db.insert_queue(&Queue {
        id: "queue-1".to_string(),
        name: "queue-1".to_string(),
        created_at: Utc::now(),
    })
    .unwrap();
    db.append_queue_member("queue-1", &spec.id, None).unwrap();

    let node = sample_loop_node(&spec.id, "node-stranded-healthy", 1);
    db.insert_loop_node(&node).unwrap();
    // Genuinely still in flight: carries the *current* boot id, so it must
    // never be treated as interrupted, and its worktree must not be touched.
    db.insert_loop_run(&LoopNodeRun {
        id: "run-stranded-healthy".to_string(),
        loop_id: lp.id,
        spec_id: spec.id,
        node_id: node.id,
        status: LoopRunStatus::Running,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: None,
        iteration: 1,
        pid: None,
        boot_id: Some(crate::system::boot_id().unwrap_or_default()),
        session_id: None,
    })
    .unwrap();

    assert_eq!(
        db.reconcile_stranded_queue_specs().unwrap(),
        0,
        "a genuinely live run must not be reconciled"
    );

    assert!(
        !git_is_clean(dir.path()),
        "a healthy run's worktree changes must never be touched"
    );

    let run_after = db.get_loop_run("run-stranded-healthy").unwrap().unwrap();
    assert_eq!(run_after.status, LoopRunStatus::Running);
}

#[test]
fn list_project_history_merges_finished_loops_and_past_sessions_newest_first() {
    use crate::db::project::ProjectHistoryKind;

    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    let db = Database::new(&path).expect("create test db");
    let workdir = "/tmp/history-project";
    let other_workdir = "/tmp/other-project";

    // A finished loop, scoped to our workdir — the oldest event.
    let mut finished_loop = sample_loop("loop-finished");
    finished_loop.workdir = workdir.to_string();
    finished_loop.name = "Finished loop".to_string();
    finished_loop.status = LoopStatus::Completed;
    finished_loop.completed_at = Some(Utc.with_ymd_and_hms(2024, 1, 1, 0, 1, 0).unwrap());
    db.insert_loop(&finished_loop).unwrap();

    // A still-running loop in the same workdir must NOT show up in history —
    // only completed/failed loops are "finished".
    let mut running_loop = sample_loop("loop-running");
    running_loop.workdir = workdir.to_string();
    running_loop.status = LoopStatus::Running;
    db.insert_loop(&running_loop).unwrap();

    // A finished loop in a *different* workdir must not leak into our results.
    let mut other_loop = sample_loop("loop-other-project");
    other_loop.workdir = other_workdir.to_string();
    other_loop.status = LoopStatus::Completed;
    other_loop.completed_at = Some(Utc.with_ymd_and_hms(2024, 1, 1, 0, 1, 30).unwrap());
    db.insert_loop(&other_loop).unwrap();

    // A past (closed) interactive session in our workdir — the middle event.
    db.insert_interactive_session(
        "session-past",
        "Past session",
        "opencode",
        workdir,
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();
    db.mark_session_closed("session-past").unwrap();

    // A still-active interactive session must NOT show up in history.
    db.insert_interactive_session(
        "session-active",
        "Active session",
        "opencode",
        workdir,
        None,
        None,
        "interactive",
        None,
    )
    .unwrap();

    // A finished terminal session in our workdir, the most recent event.
    db.insert_terminal_session("term-finished", "Finished terminal", "bash", workdir)
        .unwrap();
    db.finish_terminal_session("term-finished").unwrap();

    // A still-idle terminal session must NOT show up in history.
    db.insert_terminal_session("term-idle", "Idle terminal", "bash", workdir)
        .unwrap();

    // Pin down exact timestamps for the two session rows via a short-lived raw
    // connection: `list_project_history` truncates rfc3339 timestamps to whole
    // seconds (`DateTime::timestamp()`), so two `Utc::now()` calls made back to
    // back in this test could otherwise tie and make the ordering assertions
    // flaky. Deterministic timestamps make the newest-first order exact.
    {
        let conn = rusqlite::Connection::open(&path).expect("open raw conn for timestamp fixup");
        conn.execute(
            "UPDATE interactive_sessions SET started_at = ?1 WHERE id = 'session-past'",
            rusqlite::params!["2024-01-01T00:02:00Z"],
        )
        .unwrap();
        conn.execute(
            "UPDATE terminal_sessions SET last_active = ?1 WHERE id = 'term-finished'",
            rusqlite::params!["2024-01-01T00:03:00Z"],
        )
        .unwrap();
    }

    let history = db.list_project_history(workdir, 100).unwrap();

    assert_eq!(
        history.len(),
        3,
        "only the finished loop and the two closed/finished sessions for this workdir should appear: {:?}",
        history.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    assert!(history.iter().any(|e| e.name == "Finished loop"
        && e.kind == ProjectHistoryKind::Loop
        && e.status == "completed"));
    assert!(history
        .iter()
        .any(|e| e.name == "Past session" && e.kind == ProjectHistoryKind::InteractiveSession));
    assert!(history
        .iter()
        .any(|e| e.name == "Finished terminal" && e.kind == ProjectHistoryKind::TerminalSession));

    // Newest-first ordering: the terminal session finished most recently,
    // then the interactive session was closed, then the loop completed.
    assert_eq!(history[0].name, "Finished terminal");
    assert_eq!(history[1].name, "Past session");
    assert_eq!(history[2].name, "Finished loop");

    // Sessions/loops for other workdirs or still-active never leak in.
    assert!(!history.iter().any(|e| e.name.contains("other-project")));
    assert!(!history.iter().any(|e| e.name == "Active session"));
    assert!(!history.iter().any(|e| e.name == "Idle terminal"));
}

// ── Archiving (F4): archive/restore preserve identity and history ──────

/// The property that matters most: archiving a loop with real run history,
/// then restoring it, must leave that history byte-for-byte intact — not
/// just flip the `archived` flag. Archiving is deliberately a flag flip, not
/// a delete-and-recreate or a move to another table, so the loop's id and
/// its specs/nodes/runs (all foreign-keyed to that id) never move either.
#[test]
fn archive_then_restore_preserves_identity_and_run_history() {
    let db = test_db();
    let lp = sample_loop("wf-archive");
    let spec = sample_loop_spec(&lp.id, "spec-archive", 1);
    let node = sample_loop_node(&spec.id, "node-archive", 1);
    let run = LoopNodeRun {
        id: "run-archive".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Pass,
        input: Some(serde_json::json!({"feedback": "prior attempt"})),
        output: Some(serde_json::json!({"summary": "diagnosed the failure"})),
        started_at: Utc::now(),
        completed_at: Some(Utc::now()),
        iteration: 3,
        pid: None,
        boot_id: Some("boot-archive".to_string()),
        session_id: Some("session-archive".to_string()),
    };
    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();

    let outcome = db.archive_loop(&lp.id).unwrap();
    assert_eq!(outcome, crate::domain::loops::ArchiveLoopOutcome::Archived);

    // Excluded from the browsing listing (the query itself filters, not an
    // in-memory pass) ...
    assert!(!db
        .list_loops(None, false)
        .unwrap()
        .iter()
        .any(|l| l.id == lp.id));
    // ... but still resolvable directly by id, and it kept its own id (no
    // delete-and-recreate).
    let archived = db.get_loop(&lp.id).unwrap().unwrap();
    assert_eq!(archived.id, lp.id);
    assert!(archived.archived);
    // ... and included when a caller explicitly asks for archived loops too.
    assert!(db
        .list_loops(None, true)
        .unwrap()
        .iter()
        .any(|l| l.id == lp.id));

    // The run history — the entire point of archiving over deleting — is
    // untouched: same spec, same node, same run with its exact payloads.
    let specs = db.list_loop_specs(&lp.id).unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].id, spec.id);
    let runs = db.list_loop_runs_for_spec(&spec.id).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);
    assert_eq!(runs[0].iteration, 3);
    assert_eq!(runs[0].session_id.as_deref(), Some("session-archive"));
    assert_eq!(
        runs[0]
            .output
            .as_ref()
            .and_then(|value| value.get("summary")),
        Some(&serde_json::json!("diagnosed the failure"))
    );

    // Restoring flips the flag back and the loop reappears in the main
    // listing — still the same row, still the same history.
    assert!(db.restore_loop(&lp.id).unwrap());
    let restored = db.get_loop(&lp.id).unwrap().unwrap();
    assert!(!restored.archived);
    assert!(db
        .list_loops(None, false)
        .unwrap()
        .iter()
        .any(|l| l.id == lp.id));
    let runs_after_restore = db.list_loop_runs_for_spec(&spec.id).unwrap();
    assert_eq!(runs_after_restore.len(), 1);
    assert_eq!(runs_after_restore[0].id, run.id);
}

#[test]
fn count_archived_loops_reflects_archive_state() {
    let db = test_db();
    let a = sample_loop("wf-count-a");
    let b = sample_loop("wf-count-b");
    db.insert_loop(&a).unwrap();
    db.insert_loop(&b).unwrap();
    assert_eq!(db.count_archived_loops().unwrap(), 0);

    db.archive_loop(&a.id).unwrap();
    assert_eq!(db.count_archived_loops().unwrap(), 1);

    db.archive_loop(&b.id).unwrap();
    assert_eq!(db.count_archived_loops().unwrap(), 2);

    db.restore_loop(&a.id).unwrap();
    assert_eq!(db.count_archived_loops().unwrap(), 1);
}

#[test]
fn archive_loop_refuses_a_running_loop() {
    let db = test_db();
    let mut lp = sample_loop("wf-running");
    lp.status = LoopStatus::Running;
    db.insert_loop(&lp).unwrap();

    let outcome = db.archive_loop(&lp.id).unwrap();
    assert_eq!(outcome, crate::domain::loops::ArchiveLoopOutcome::Running);

    // Refused, not silently ignored: the loop is still in the main listing.
    let reloaded = db.get_loop(&lp.id).unwrap().unwrap();
    assert!(!reloaded.archived);
    assert_eq!(db.count_archived_loops().unwrap(), 0);
}

#[test]
fn archive_loop_already_archived_reports_already_archived() {
    let db = test_db();
    let lp = sample_loop("wf-double-archive");
    db.insert_loop(&lp).unwrap();

    assert_eq!(
        db.archive_loop(&lp.id).unwrap(),
        crate::domain::loops::ArchiveLoopOutcome::Archived
    );
    assert_eq!(
        db.archive_loop(&lp.id).unwrap(),
        crate::domain::loops::ArchiveLoopOutcome::AlreadyArchived
    );
}

#[test]
fn restore_loop_not_archived_is_a_noop() {
    let db = test_db();
    let lp = sample_loop("wf-not-archived");
    db.insert_loop(&lp).unwrap();

    assert!(!db.restore_loop(&lp.id).unwrap());
    assert!(!db.restore_loop("ghost-loop").unwrap());
}

/// Permanent deletion — reachable only from the archive on an
/// already-archived loop — must actually destroy the run history it warns
/// about, unlike archiving. Exercises the same cascade `ON DELETE CASCADE`
/// relationships archiving is designed to never touch.
#[test]
fn permanent_delete_removes_the_loop_and_its_run_history() {
    let db = test_db();
    let lp = sample_loop("wf-permanent-delete");
    let spec = sample_loop_spec(&lp.id, "spec-permanent-delete", 1);
    let node = sample_loop_node(&spec.id, "node-permanent-delete", 1);
    let run = LoopNodeRun {
        id: "run-permanent-delete".to_string(),
        loop_id: lp.id.clone(),
        spec_id: spec.id.clone(),
        node_id: node.id.clone(),
        status: LoopRunStatus::Fail,
        input: None,
        output: None,
        started_at: Utc::now(),
        completed_at: Some(Utc::now()),
        iteration: 1,
        pid: None,
        boot_id: None,
        session_id: None,
    };
    db.insert_loop(&lp).unwrap();
    db.insert_loop_spec(&spec).unwrap();
    db.insert_loop_node(&node).unwrap();
    db.insert_loop_run(&run).unwrap();
    db.archive_loop(&lp.id).unwrap();

    db.delete_loop(&lp.id).unwrap();

    assert!(db.get_loop(&lp.id).unwrap().is_none());
    assert!(db.list_loop_specs(&lp.id).unwrap().is_empty());
    assert!(db.get_loop_run(&run.id).unwrap().is_none());
}

/// Older databases predate the `archived` column entirely. Opening one
/// (`Database::new` runs the migration) must add the column, defaulting
/// every existing row to not-archived with no data movement, and the
/// migration must be a no-op on a second open.
#[test]
fn archived_migration_defaults_existing_rows_and_is_idempotent() {
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
                spec_queue TEXT,
                active_run_queue_id TEXT,
                on_completed TEXT,
                auto_continue_at INTEGER,
                auto_continue_action TEXT
             );
             INSERT INTO loops (id, name, workdir, status, created_at)
                 VALUES ('legacy-loop', 'Legacy', '/tmp', 'completed', 0);",
        )
        .expect("seed legacy schema");
    }

    let db = Database::new(&path).expect("open pre-archived db, running migration");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert_eq!(lp.name, "Legacy");
    assert!(
        !lp.archived,
        "pre-existing rows must default to not archived"
    );

    // The new column is actually usable after migration.
    assert_eq!(
        db.archive_loop("legacy-loop").unwrap(),
        crate::domain::loops::ArchiveLoopOutcome::Archived
    );
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent), archived state preserved.
    let db = Database::new(&path).expect("reopen db after migration already applied");
    let lp = db.get_loop("legacy-loop").unwrap().unwrap();
    assert!(lp.archived);
}

#[test]
fn cross_run_attempts_migration_defaults_existing_rows_and_is_idempotent() {
    // Simulate a pre-C19 database: `loop_specs` without `cross_run_attempts`.
    let tmp = NamedTempFile::new().expect("create temp file");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    {
        let conn = rusqlite::Connection::open(&path).expect("open raw legacy db");
        conn.execute_batch(
            "CREATE TABLE loop_specs (
                id TEXT PRIMARY KEY,
                loop_id TEXT,
                name TEXT NOT NULL,
                description TEXT,
                position INTEGER NOT NULL,
                parallelizable INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                spec_start_head TEXT,
                workdir TEXT,
                completed_via TEXT,
                completed_via_reason TEXT,
                completed_via_at INTEGER,
                spec_committed_head TEXT
             );
             INSERT INTO loop_specs (id, loop_id, name, position, status)
                 VALUES ('legacy-spec', NULL, 'Spec', 1, 'failed');",
        )
        .expect("seed legacy schema");
    }

    // Opening the DB (Database::new runs the migration) must add the column
    // without erroring, defaulting the pre-existing row to zero attempts.
    let db = Database::new(&path).expect("open pre-cross_run_attempts db, running migration");
    assert_eq!(
        db.get_loop_spec_cross_run_attempts("legacy-spec").unwrap(),
        0,
        "pre-existing rows must default to zero attempts"
    );

    // The new column is actually usable after migration.
    assert_eq!(
        db.increment_loop_spec_cross_run_attempts("legacy-spec")
            .unwrap(),
        1
    );
    drop(db);

    // Reopening after the migration already ran must be a no-op: same data,
    // no error (idempotent), the incremented count preserved.
    let db = Database::new(&path).expect("reopen db after migration already applied");
    assert_eq!(
        db.get_loop_spec_cross_run_attempts("legacy-spec").unwrap(),
        1
    );
}

// RETIRED-SCHEMA-NAME-BEGIN
/// Guards the pool-to-queue rename from regressing a third time: the public
/// surface and the schema were already renamed once each, in two separate
/// passes, which is exactly how this codebase ended up with a schema still
/// naming the retired concept. Every `.rs` file under `src/` is scanned for
/// the retired name as a whole word or a `snake_case` suffix; a hit is
/// exempt only between a `RETIRED-SCHEMA-NAME-BEGIN` / `-END` marker pair,
/// which brackets the legacy migration, its tests, and this guard itself —
/// all three must name the retired schema literally to do their job.
#[test]
fn no_retired_schema_name_identifiers_remain_outside_its_migration() {
    // `\b` treats `_` as a word char, so it won't fire between `_` and
    // `pool` (e.g. `test_pool_marker`). Normalizing `_` to a space first
    // turns every snake_case segment boundary into a real `\b`, so one
    // simple pattern catches prefix (`PoolMember`), suffix (`spec_pool`),
    // and mid-identifier (`test_pool_marker`) forms alike.
    let retired_name = regex::Regex::new(r"(?i)\bpool[a-zA-Z]*").unwrap();
    let begin_marker = "RETIRED-SCHEMA-NAME-BEGIN";
    let end_marker = "RETIRED-SCHEMA-NAME-END";
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src_dir = std::path::Path::new(manifest_dir).join("src");

    let mut violations = Vec::new();
    for entry in walkdir::WalkDir::new(&src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let rel_path = entry
            .path()
            .strip_prefix(manifest_dir)
            .unwrap_or(entry.path())
            .to_path_buf();
        let source = std::fs::read_to_string(entry.path())
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", rel_path.display()));

        let mut exempt = false;
        for (i, line) in source.lines().enumerate() {
            if line.contains(end_marker) {
                exempt = false;
                continue;
            }
            if line.contains(begin_marker) {
                exempt = true;
                continue;
            }
            if exempt {
                continue;
            }
            let normalized = line.replace('_', " ");
            if let Some(m) = retired_name.find(&normalized) {
                violations.push(format!("{}:{} — {}", rel_path.display(), i + 1, m.as_str()));
            }
        }
        assert!(
            !exempt,
            "{} has an unclosed {begin_marker} region",
            rel_path.display()
        );
    }

    assert!(
        violations.is_empty(),
        "retired schema name found outside a RETIRED-SCHEMA-NAME-BEGIN/-END \
         region (queue is the decided term — see migrate_legacy_queue_schema \
         for the one place the retired name may still appear):\n{}",
        violations.join("\n")
    );
}
// RETIRED-SCHEMA-NAME-END

#[test]
fn ensemble_kind_migration_is_idempotent_and_pre_migration_db_opens_cleanly() {
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
                status TEXT NOT NULL DEFAULT 'pending',
                created_at INTEGER NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                trigger_kind TEXT,
                trigger_schedule TEXT,
                trigger_path TEXT,
                trigger_events TEXT,
                trigger_debounce_seconds INTEGER,
                trigger_recursive INTEGER,
                on_completed_hook_platform TEXT,
                on_completed_hook_prompt TEXT,
                on_completed_hook_model TEXT,
                on_completed_hook_timeout_minutes INTEGER
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
                spec_start_head TEXT,
                workdir TEXT,
                completed_via TEXT,
                completed_via_reason TEXT,
                completed_via_at INTEGER,
                spec_committed_head TEXT
            );
            CREATE TABLE loop_nodes (
                id TEXT PRIMARY KEY,
                spec_id TEXT REFERENCES loop_specs(id) ON DELETE CASCADE,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                config TEXT NOT NULL,
                position INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE loop_edges (
                id TEXT PRIMARY KEY,
                spec_id TEXT REFERENCES loop_specs(id) ON DELETE CASCADE,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                from_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                to_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                condition TEXT NOT NULL
            );
            CREATE TABLE ensembles (
                id TEXT PRIMARY KEY,
                spec_id TEXT REFERENCES loop_specs(id) ON DELETE CASCADE,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                prompt_template TEXT NOT NULL,
                join_node_id TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                entry_from_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                entry_condition TEXT NOT NULL,
                min_pass INTEGER NOT NULL,
                straggler_timeout_minutes INTEGER,
                timeout_minutes INTEGER NOT NULL,
                on_pass_to TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                on_fail_to TEXT REFERENCES loop_nodes(id) ON DELETE SET NULL,
                created_at INTEGER NOT NULL,
                CHECK ((spec_id IS NULL) <> (loop_id IS NULL))
            );",
        )
        .expect("create legacy schema");
    }

    let db = Database::new(&path).expect("open pre-ensemble-kind db, running migration");

    let has_kind: bool = {
        let conn = rusqlite::Connection::open(&path).expect("reopen");
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('ensembles') WHERE name = 'kind'",
            [],
            |row| Ok(row.get::<_, i32>(0)? > 0),
        )
        .unwrap()
    };
    assert!(has_kind, "migration should have added 'kind' column");

    let has_rri: bool = {
        let conn = rusqlite::Connection::open(&path).expect("reopen");
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('ensembles') WHERE name = 'round_robin_index'",
            [],
            |row| Ok(row.get::<_, i32>(0)? > 0),
        )
        .unwrap()
    };
    assert!(
        has_rri,
        "migration should have added 'round_robin_index' column"
    );

    drop(db);
    let _db2 = Database::new(&path).expect("reopen after migration — idempotent");
}

#[test]
fn insert_and_collect_subagent_run() {
    let db = test_db();
    let now = Utc::now().to_rfc3339();
    let expires = (Utc::now() + Duration::hours(1)).to_rfc3339();
    db.insert_subagent_run(
        "run-1",
        "opencode",
        Some("model-x"),
        "do stuff",
        "/tmp",
        &now,
        &expires,
    )
    .unwrap();
    db.complete_subagent_run("run-1", 0, "hello", "", Some("blind"), &now)
        .unwrap();

    let record = db.collect_subagent_run("run-1").unwrap();
    assert!(record.is_some());
    let r = record.unwrap();
    assert_eq!(r.id, "run-1");
    assert_eq!(r.status, "finished");
    assert_eq!(r.stdout.as_deref(), Some("hello"));

    let second = db.collect_subagent_run("run-1").unwrap();
    assert!(second.is_none(), "row must be deleted after collection");
}

#[test]
fn collect_while_running_does_not_discard_the_result() {
    let db = test_db();
    let now = Utc::now().to_rfc3339();
    let expires = (Utc::now() + Duration::hours(1)).to_rfc3339();
    db.insert_subagent_run("run-2", "opencode", None, "p", "/tmp", &now, &expires)
        .unwrap();

    // A poll before the subagent finishes reports "running" but must NOT
    // delete the row — otherwise the eventual result is lost forever.
    let early = db.collect_subagent_run("run-2").unwrap().unwrap();
    assert_eq!(early.status, "running");

    db.complete_subagent_run("run-2", 0, "the answer", "", Some("blind"), &now)
        .unwrap();

    let collected = db.collect_subagent_run("run-2").unwrap().unwrap();
    assert_eq!(collected.status, "finished");
    assert_eq!(collected.stdout.as_deref(), Some("the answer"));
    assert!(
        db.collect_subagent_run("run-2").unwrap().is_none(),
        "row is discarded only after a terminal result is collected"
    );
}

#[test]
fn expire_subagent_runs_deletes_expired() {
    let db = test_db();
    let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
    let now = Utc::now().to_rfc3339();
    db.insert_subagent_run("run-exp", "opencode", None, "p", "/tmp", &now, &past)
        .unwrap();
    let deleted = db.expire_subagent_runs().unwrap();
    assert_eq!(deleted, 1);
    assert!(db.get_subagent_run("run-exp").unwrap().is_none());
}

#[test]
fn expire_subagent_runs_keeps_unexpired() {
    let db = test_db();
    let now = Utc::now().to_rfc3339();
    let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
    db.insert_subagent_run("run-keep", "opencode", None, "p", "/tmp", &now, &future)
        .unwrap();
    let deleted = db.expire_subagent_runs().unwrap();
    assert_eq!(deleted, 0);
    assert!(db.get_subagent_run("run-keep").unwrap().is_some());
}

#[test]
fn fail_subagent_run_sets_status() {
    let db = test_db();
    let now = Utc::now().to_rfc3339();
    let expires = (Utc::now() + Duration::hours(1)).to_rfc3339();
    db.insert_subagent_run("run-fail", "opencode", None, "p", "/tmp", &now, &expires)
        .unwrap();
    db.fail_subagent_run("run-fail", "oops", Some("blind"), &now)
        .unwrap();
    let record = db.get_subagent_run("run-fail").unwrap().unwrap();
    assert_eq!(record.status, "failed");
    assert_eq!(record.stderr.as_deref(), Some("oops"));
    assert!(record.exit_code.is_none());
}
