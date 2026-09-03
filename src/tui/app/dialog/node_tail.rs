//! CT3: live tail dialog for a running check node.
//!
//! Diagnostic-only viewer over the `loop_run_output` chunks the engine
//! streams while the node runs (see `spawn_check_output_reader`). Opening
//! and closing the dialog never touches the run row — closing cannot affect
//! execution by construction, since every dialog read is a `SELECT`.
//!
//! Open questions (decided before implementing, per the spec):
//! 1. **PTY vs accumulated output:** accumulated output via the existing
//!    `Stdio::piped()` stdout/stderr pipes. No PTY — zero spawn changes,
//!    exact harness bytes (ANSI escapes, partial lines, zero-output hangs).
//! 2. **Node finishes with dialog open:** the dialog stays open with a
//!    final status banner (`passed` / `failed` / `timed out`); `Esc`/`t`
//!    dismisses it. Closing never affects execution.
//! 3. **Not-yet-started nodes:** the dialog only opens for `Running` runs.
//!    Pending nodes have no output, so an empty pane would have no
//!    diagnostic value; `open_node_tail_dialog` returns `false` otherwise.

use chrono::{DateTime, Utc};

use crate::domain::loops::LoopRunStatus;
use crate::tui::app::types::App;

/// Lines retained per stream in the dialog — bounds TUI memory when
/// following a very chatty node (non-functional requirement).
pub(crate) const NODE_TAIL_MAX_LINES: usize = 1000;

/// Live-follow state for one running node. A plain viewer: no handles, no
/// child references, nothing that could signal the engine on close.
pub(crate) struct NodeTailDialog {
    pub run_id: String,
    /// Kept for run↔node correlation in future controls; the title shows
    /// `node_name` instead. Today nothing reads it after `open`.
    #[allow(dead_code)]
    pub node_id: String,
    pub node_name: String,
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
    pub status: LoopRunStatus,
    /// True when the finished run's output JSON carries `"error": "timed
    /// out"` — distinguishes a silent-timeout hang from a plain failure.
    pub timed_out: bool,
    pub started_at: DateTime<Utc>,
    /// Lines scrolled up from the live bottom edge (0 = following live).
    pub scroll: usize,
}

impl App {
    /// Open the tail dialog for `node_id`. Only succeeds while the node has
    /// a `Running` run row — i.e. it is actually executing right now.
    /// Returns `false` for pending/finished/unknown nodes (no empty panes).
    pub fn open_node_tail_dialog(&mut self, node_id: &str) -> bool {
        let Ok(Some(run)) = self.db.get_active_loop_run_for_node(node_id) else {
            return false;
        };
        if run.status != LoopRunStatus::Running {
            return false;
        }
        let node_name = self
            .loop_live_state
            .as_ref()
            .and_then(|state| {
                state
                    .effective_nodes
                    .iter()
                    .find(|n| n.id == node_id)
                    .map(|n| n.name.clone())
            })
            .unwrap_or_else(|| node_id.to_string());
        self.node_tail_dialog = Some(NodeTailDialog {
            run_id: run.id,
            node_id: node_id.to_string(),
            node_name,
            stdout_lines: Vec::new(),
            stderr_lines: Vec::new(),
            status: run.status,
            timed_out: false,
            started_at: run.started_at,
            scroll: 0,
        });
        self.poll_node_tail_dialog();
        true
    }

    /// Dismiss the dialog. Read-only by construction — clears viewer state
    /// only; the run row and the engine are untouched.
    pub fn close_node_tail_dialog(&mut self) {
        self.node_tail_dialog = None;
    }

    pub fn node_tail_dialog_active(&self) -> bool {
        self.node_tail_dialog.is_some()
    }

    /// Scroll the open dialog: `down == true` moves toward live (newer
    /// lines), `false` moves back toward older lines. Clamped at render
    /// time against the actual line count.
    pub fn node_tail_scroll(&mut self, down: bool) {
        if let Some(dialog) = self.node_tail_dialog.as_mut() {
            if down {
                dialog.scroll = dialog.scroll.saturating_sub(3);
            } else {
                dialog.scroll = dialog.scroll.saturating_add(3);
            }
        }
    }

    /// Refresh the open dialog from the DB: latest streamed chunks (capped
    /// at [`NODE_TAIL_MAX_LINES`] lines per stream) plus the run's current
    /// status, so a finishing node flips to its final banner on the next
    /// tick. A hung node shows its captured output frozen with the `Running`
    /// badge and growing elapsed time — visibly hung, never blank.
    pub(crate) fn poll_node_tail_dialog(&mut self) {
        let run_id = match self.node_tail_dialog.as_ref() {
            Some(dialog) => dialog.run_id.clone(),
            None => return,
        };
        let tail = self.db.get_loop_run_tail(&run_id, NODE_TAIL_MAX_LINES).ok();
        let run = self.db.get_loop_run(&run_id).ok().flatten();
        if let Some(dialog) = self.node_tail_dialog.as_mut() {
            if let Some((stdout, stderr)) = tail {
                dialog.stdout_lines = stdout.lines().map(str::to_string).collect();
                dialog.stderr_lines = stderr.lines().map(str::to_string).collect();
            }
            if let Some(run) = run {
                dialog.status = run.status;
                dialog.timed_out = run
                    .output
                    .as_ref()
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|e| e == "timed out");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::loops::{
        Loop, LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus,
        LoopStatus,
    };
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn seed_running_node(db: &Database) {
        db.insert_loop(&Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
            id: "lp-tail".to_string(),
            name: "Loop lp-tail".to_string(),
            description: None,
            workdir: "/tmp/test".to_string(),
            status: LoopStatus::Running,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        })
        .unwrap();
        db.insert_loop_spec(&LoopSpec {
            id: "spec-tail".to_string(),
            loop_id: Some("lp-tail".to_string()),
            name: "Spec".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Running,
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
        db.insert_loop_node(&LoopNode {
            id: "node-tail".to_string(),
            spec_id: Some("spec-tail".to_string()),
            loop_id: None,
            name: "Tail node".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true"}),
            position: 0,
            created_at: Utc::now(),
        })
        .unwrap();
        db.insert_loop_run(&LoopNodeRun {
            id: "run-tail".to_string(),
            loop_id: "lp-tail".to_string(),
            spec_id: "spec-tail".to_string(),
            node_id: "node-tail".to_string(),
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
    }

    fn test_app(db: Arc<Database>) -> App {
        let data_dir = tempdir().expect("create data dir");
        // App::new refreshes from the DB; the temp dir only holds TUI-side
        // caches, so leaking it for the test's lifetime is fine.
        let dir_path = data_dir.path().to_path_buf();
        std::mem::forget(data_dir);
        App::new(db, &dir_path).expect("create app")
    }

    #[test]
    fn open_tail_only_for_running() {
        let db = test_db();
        seed_running_node(&db);
        let mut app = test_app(Arc::clone(&db));

        assert!(app.open_node_tail_dialog("node-tail"));
        assert!(app.node_tail_dialog_active());
        let dialog = app.node_tail_dialog.as_ref().unwrap();
        assert_eq!(dialog.run_id, "run-tail");

        // Finish the run: the gate closes — no tail for finished nodes.
        app.close_node_tail_dialog();
        db.update_loop_run_result(
            "run-tail",
            LoopRunStatus::Fail,
            Some(&serde_json::json!({"kind": "check", "passed": false})),
            Some(Utc::now()),
        )
        .unwrap();
        assert!(!app.open_node_tail_dialog("node-tail"));
        assert!(!app.node_tail_dialog_active());

        // Unknown nodes never open.
        assert!(!app.open_node_tail_dialog("node-missing"));
    }

    #[test]
    fn close_tail_does_not_affect_run() {
        let db = test_db();
        seed_running_node(&db);
        let mut app = test_app(Arc::clone(&db));

        assert!(app.open_node_tail_dialog("node-tail"));
        db.append_loop_run_output("run-tail", "stdout", "work in progress\n")
            .unwrap();
        app.close_node_tail_dialog();

        assert!(!app.node_tail_dialog_active());
        let run = db.get_loop_run("run-tail").unwrap().unwrap();
        assert_eq!(run.status, LoopRunStatus::Running);
        let (stdout, _) = db.get_loop_run_tail("run-tail", 1000).unwrap();
        assert!(stdout.contains("work in progress"));
    }

    #[test]
    fn poll_picks_up_streamed_output_and_finish() {
        let db = test_db();
        seed_running_node(&db);
        let mut app = test_app(Arc::clone(&db));

        assert!(app.open_node_tail_dialog("node-tail"));
        db.append_loop_run_output("run-tail", "stdout", "hello\n")
            .unwrap();
        app.poll_node_tail_dialog();
        assert_eq!(
            app.node_tail_dialog.as_ref().unwrap().stdout_lines,
            vec!["hello".to_string()]
        );
        assert_eq!(
            app.node_tail_dialog.as_ref().unwrap().status,
            LoopRunStatus::Running
        );

        // Node finishes (timeout): the open dialog flips to the final
        // banner instead of going stale.
        db.update_loop_run_result(
            "run-tail",
            LoopRunStatus::Fail,
            Some(&serde_json::json!({"kind": "check", "error": "timed out"})),
            Some(Utc::now()),
        )
        .unwrap();
        app.poll_node_tail_dialog();
        let dialog = app.node_tail_dialog.as_ref().unwrap();
        assert_eq!(dialog.status, LoopRunStatus::Fail);
        assert!(dialog.timed_out);
        // Still open — the user dismisses it, not the engine.
        assert!(app.node_tail_dialog_active());
    }

    #[test]
    fn scroll_moves_away_from_and_back_to_live() {
        let db = test_db();
        seed_running_node(&db);
        let mut app = test_app(Arc::clone(&db));

        assert!(app.open_node_tail_dialog("node-tail"));
        app.node_tail_scroll(false);
        assert_eq!(app.node_tail_dialog.as_ref().unwrap().scroll, 3);
        app.node_tail_scroll(true);
        assert_eq!(app.node_tail_dialog.as_ref().unwrap().scroll, 0);
        // Scrolling with no dialog open is a no-op, never a panic.
        app.close_node_tail_dialog();
        app.node_tail_scroll(false);
    }
}
