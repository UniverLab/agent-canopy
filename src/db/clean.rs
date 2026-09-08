//! Repository functions backing `canopy clean` (soft cleanup, C1) and
//! `canopy clean --hard` (orphan-project cascade, C2).
//!
//! Query/mutate helpers only — the decision of *what* is safe to remove
//! lives in `domain::clean` as pure functions over the facts these return.

use anyhow::Result;
use rusqlite::params;
use std::collections::HashSet;

use crate::db::Database;
use crate::domain::clean::{
    HardCascadeCounts, HardCascadeSkipReason, ProjectDependentCounts, SessionCandidate,
};

fn parse_rfc3339_ts(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

impl Database {
    /// `interactive_sessions` rows in the only statuses `canopy clean` (soft
    /// mode) is ever allowed to remove. `active` and `resumed` rows are
    /// excluded by this query itself, not just by the caller's later
    /// filtering, so a bug downstream can't widen the blast radius to a live
    /// or just-resumed session.
    pub fn list_cleanable_interactive_sessions(&self) -> Result<Vec<SessionCandidate>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, status, COALESCE(exited_at, started_at)
             FROM interactive_sessions
             WHERE status IN ('orphaned', 'error', 'completed')",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let status: String = row.get(1)?;
                let at: String = row.get(2)?;
                Ok((id, status, at))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(id, status, at)| SessionCandidate {
                id,
                status,
                age_ts: parse_rfc3339_ts(&at),
            })
            .collect())
    }

    /// Batch-delete `interactive_sessions` rows by id inside a single
    /// transaction, so a clean interrupted partway through can't leave the
    /// DB with only some of the planned rows gone.
    pub fn delete_interactive_sessions(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let tx = conn.transaction()?;
        let mut deleted = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM interactive_sessions WHERE id = ?1")?;
            for id in ids {
                deleted += stmt.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// All registered background-agent ids — cross-referenced against
    /// `logs/<id>.log` filenames to detect orphaned log files (an agent
    /// removed via `agent_remove` leaves its log file behind).
    pub fn list_agent_ids(&self) -> Result<HashSet<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT id FROM agents")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(rows)
    }

    /// All distinct `terminal_sessions.name` values — cross-referenced
    /// against `terminals/<name>/` directory names (terminal history is
    /// keyed by session *name*, not id) to detect orphaned history dirs.
    pub fn list_terminal_session_names(&self) -> Result<HashSet<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT DISTINCT name FROM terminal_sessions")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(rows)
    }

    /// Row counts that depend on a project's workdir, surfaced in the
    /// orphaned-project report.
    pub fn project_dependent_counts(&self, workdir: &str) -> Result<ProjectDependentCounts> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let loops: i64 = conn.query_row(
            "SELECT COUNT(*) FROM loops WHERE workdir = ?1",
            params![workdir],
            |row| row.get(0),
        )?;
        let interactive_sessions: i64 = conn.query_row(
            "SELECT COUNT(*) FROM interactive_sessions WHERE working_dir = ?1",
            params![workdir],
            |row| row.get(0),
        )?;
        let terminal_sessions: i64 = conn.query_row(
            "SELECT COUNT(*) FROM terminal_sessions WHERE working_dir = ?1",
            params![workdir],
            |row| row.get(0),
        )?;
        Ok(ProjectDependentCounts {
            loops,
            interactive_sessions,
            terminal_sessions,
        })
    }

    /// Full per-table row counts for the `--hard` cascade plan. Counts both
    /// the direct targets (rows whose own `workdir`/`working_dir`/
    /// `project_hash` column points at this project) and the FK-CASCADE
    /// follow-on rows that the database will auto-remove once the direct
    /// target is deleted. The follow-on counts are needed so the printed
    /// plan can show the real blast radius (a loop with a 200-node graph
    /// deletes 200 more rows than just the loop row itself).
    ///
    /// Single transaction at READ COMMITTED (no writes) so the counts are
    /// internally consistent: a row counted in `loop_specs` here cannot
    /// have disappeared from `loops` between the two queries.
    ///
    /// `hash` is the project's `projects.hash` (what `intelligence_nodes.
    /// project_hash` stores) — distinct from `workdir`, which every other
    /// project-scoped table keys on. Passing `workdir` for both would
    /// silently under-count intelligence rows for every real project.
    pub fn project_hard_cascade_counts(
        &self,
        hash: &str,
        workdir: &str,
    ) -> Result<HardCascadeCounts> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let tx = conn.unchecked_transaction()?;
        let counts = count_hard_cascade(&tx, hash, workdir)?;
        tx.commit()?;
        Ok(counts)
    }

    /// Transactional cascade delete of one orphaned project. Returns the
    /// `HardCascadeCounts` of rows actually deleted, so the caller can
    /// print "removed N rows" after the prompt-and-confirm. The cascade is
    /// expressed entirely in this function (not in the CLI layer): the
    /// counts are gathered first, then every direct-target `DELETE` is
    /// issued inside the *same* SQLite transaction, so an interrupted
    /// `--hard` can never leave the project half-deleted (C2 spec:
    /// "transactional per project: either the whole cascade for a project
    /// applies or none of it").
    ///
    /// `pragma foreign_keys=ON` is in effect (see `Database::new`), so
    /// deleting `loops` cascades through `loop_specs` (loop-bound only) /
    /// `loop_nodes` / `loop_edges` / `loop_runs` /
    /// `loop_completion_hook_runs` / `ensembles` / `ensemble_members` /
    /// `queue_members`; deleting `interactive_sessions` cascades through
    /// `seed_sessions`; deleting `intelligence_nodes` cascades through
    /// `intelligence_edges`. Those follow-on rows are therefore never
    /// deleted by an explicit statement here — issuing one *after* the
    /// cascade already ran would always affect zero rows, silently
    /// under-reporting the count. The pre-computed counts from
    /// `count_hard_cascade` are what's returned instead, since by
    /// construction every one of those rows will have been removed by the
    /// time this transaction commits.
    pub fn cascade_delete_orphan_project(
        &self,
        hash: &str,
        workdir: &str,
    ) -> Result<HardCascadeCounts> {
        let mut conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let tx = conn.transaction()?;
        let counts = count_hard_cascade(&tx, hash, workdir)?;

        tx.execute(
            "DELETE FROM sync_messages WHERE workdir = ?1",
            params![workdir],
        )?;
        tx.execute(
            "DELETE FROM sync_locks WHERE workdir = ?1",
            params![workdir],
        )?;
        tx.execute(
            "DELETE FROM last_prompts WHERE workdir = ?1",
            params![workdir],
        )?;
        tx.execute(
            "DELETE FROM scheduled_sends WHERE workdir = ?1",
            params![workdir],
        )?;
        tx.execute(
            "DELETE FROM failed_scheduled_sends WHERE workdir = ?1",
            params![workdir],
        )?;
        tx.execute(
            "DELETE FROM terminal_sessions WHERE working_dir = ?1",
            params![workdir],
        )?;
        // CASCADEs to seed_sessions.
        tx.execute(
            "DELETE FROM interactive_sessions WHERE working_dir = ?1",
            params![workdir],
        )?;
        // CASCADEs to loop-bound loop_specs, which CASCADEs further to
        // loop_nodes / loop_edges / ensembles / ensemble_members /
        // queue_members / loop_runs; loop_completion_hook_runs CASCADEs
        // directly off loop_id. Standalone specs (loop_id IS NULL) are
        // untouched, matching count_hard_cascade.
        tx.execute("DELETE FROM loops WHERE workdir = ?1", params![workdir])?;
        // CASCADEs to intelligence_edges.
        tx.execute(
            "DELETE FROM intelligence_nodes WHERE project_hash = ?1",
            params![hash],
        )?;
        tx.execute(
            "DELETE FROM operational_sessions WHERE project_hash = ?1",
            params![hash],
        )?;
        tx.execute("DELETE FROM projects WHERE hash = ?1", params![hash])?;

        tx.commit()?;
        Ok(counts)
    }

    /// Rewrites the database file to return space freed by deleted rows to
    /// the filesystem, then checkpoints the WAL into it so the `-wal` file
    /// shrinks too. Takes an exclusive lock for the duration — callers must
    /// make sure nothing else has the database open for writing (see
    /// `daemon::clean_cli`'s reclaim step, which refuses to run this while
    /// the daemon is up).
    ///
    /// A full `VACUUM` was chosen over `PRAGMA auto_vacuum=INCREMENTAL` +
    /// `incremental_vacuum`: incremental auto-vacuum only takes effect for
    /// databases created (or already fully rewritten) after the pragma is
    /// set, so an existing database — like every one `canopy clean` will
    /// ever run against — needs a one-time full rewrite regardless before
    /// incremental mode does anything. A plain `VACUUM` gets the same space
    /// back today, in one step, without also taking on a migration path and
    /// a mode that still needs a periodic manual `incremental_vacuum` call
    /// to keep paying off.
    ///
    /// If interrupted (crash, kill -9, power loss), SQLite's own commit
    /// mechanism protects the original file: `VACUUM` builds its rewritten
    /// copy in a separate temp database and only replaces the original as
    /// part of committing that transaction, so a `VACUUM` that never
    /// commits leaves the database exactly as it was — never a half
    /// re-written, unusable file.
    pub fn reclaim_space(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        // Wait out a brief writer instead of failing instantly on
        // `SQLITE_BUSY` — a short in-flight write (e.g. the TUI recording a
        // session event) shouldn't abort the whole reclaim.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// A lightweight, single-pass integrity check — good enough to catch a
    /// visibly broken file before an in-place-rewriting `VACUUM` entrenches
    /// the damage. Returns `"ok"` when the database is fine; any other
    /// string names the specific problem(s) `PRAGMA quick_check` found (it
    /// can return multiple rows, joined here with `"; "`).
    ///
    /// Deliberately `quick_check`, not the fuller, slower
    /// `PRAGMA integrity_check`: by the time `canopy clean`'s reclaim window
    /// calls this, the service is already down and this gate only needs to
    /// refuse a visibly broken file before `VACUUM` runs — the thorough scan
    /// belongs to a periodic health routine, not the critical path of a
    /// service-down window.
    pub fn quick_check(&self) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare("PRAGMA quick_check")?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.join("; "))
    }

    /// What's currently in flight that a `canopy clean --stop-daemon`
    /// reclaim window must not interrupt: a running loop, or a live
    /// interactive session (a human or TUI actively attached). Mirrors the
    /// same `status = 'running'` / `status IN ('active', 'resumed')` checks
    /// [`Self::project_hard_cascade_skip_reason`] already uses for "a human
    /// or TUI is looking at this right now", just scoped to the whole
    /// install instead of one project.
    pub fn busy_reasons(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut reasons = Vec::new();

        let running_loops: i64 = conn.query_row(
            "SELECT COUNT(*) FROM loops WHERE status = 'running'",
            [],
            |row| row.get(0),
        )?;
        if running_loops > 0 {
            reasons.push(format!("{running_loops} loop(s) currently running"));
        }

        let active_sessions: i64 = conn.query_row(
            "SELECT COUNT(*) FROM interactive_sessions WHERE status IN ('active', 'resumed')",
            [],
            |row| row.get(0),
        )?;
        if active_sessions > 0 {
            reasons.push(format!(
                "{active_sessions} interactive session(s) attached (a TUI is in use)"
            ));
        }

        Ok(reasons)
    }

    /// Why a project must be skipped by `--hard`, if any. Returns `Some` only
    /// when the project has either a `Running` loop or an `active`/`resumed`
    /// interactive session, both of which are in-flight state the cascade
    /// MUST NOT touch.
    ///
    /// Active/resumed sessions are checked first: those are the most direct
    /// "a human or TUI is looking at this right now" signal (a running loop
    /// may also be reported separately as part of the project's history).
    pub fn project_hard_cascade_skip_reason(
        &self,
        workdir: &str,
    ) -> Result<Option<HardCascadeSkipReason>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let active_session: bool = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM interactive_sessions
                 WHERE working_dir = ?1 AND status IN ('active', 'resumed')
             )",
            params![workdir],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if active_session {
            return Ok(Some(HardCascadeSkipReason::ActiveSession));
        }
        let running_loop: bool = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM loops
                 WHERE workdir = ?1 AND status = 'running'
             )",
            params![workdir],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if running_loop {
            return Ok(Some(HardCascadeSkipReason::RunningLoop));
        }
        Ok(None)
    }
}

/// Shared counting logic for [`Database::project_hard_cascade_counts`] (the
/// pre-delete plan) and [`Database::cascade_delete_orphan_project`] (which
/// counts before deleting, since rows removed by FK CASCADE can't be
/// counted by their own `DELETE` statement's affected-row count — the
/// parent's `DELETE` is what removes them). Both call sites must see
/// identical numbers, so this is the only place the counting SQL lives.
fn count_hard_cascade(
    tx: &rusqlite::Transaction,
    hash: &str,
    workdir: &str,
) -> Result<HardCascadeCounts> {
    // Direct targets — rows whose own column references the project.
    let loops: i64 = tx.query_row(
        "SELECT COUNT(*) FROM loops WHERE workdir = ?1",
        params![workdir],
        |row| row.get(0),
    )?;
    let interactive_sessions: i64 = tx.query_row(
        "SELECT COUNT(*) FROM interactive_sessions WHERE working_dir = ?1",
        params![workdir],
        |row| row.get(0),
    )?;
    let terminal_sessions: i64 = tx.query_row(
        "SELECT COUNT(*) FROM terminal_sessions WHERE working_dir = ?1",
        params![workdir],
        |row| row.get(0),
    )?;
    let last_prompts: i64 = tx.query_row(
        "SELECT COUNT(*) FROM last_prompts WHERE workdir = ?1",
        params![workdir],
        |row| row.get(0),
    )?;
    let scheduled_sends: i64 = tx.query_row(
        "SELECT COUNT(*) FROM scheduled_sends WHERE workdir = ?1",
        params![workdir],
        |row| row.get(0),
    )?;
    let failed_scheduled_sends: i64 = tx.query_row(
        "SELECT COUNT(*) FROM failed_scheduled_sends WHERE workdir = ?1",
        params![workdir],
        |row| row.get(0),
    )?;
    let sync_messages: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sync_messages WHERE workdir = ?1",
        params![workdir],
        |row| row.get(0),
    )?;
    let sync_locks: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sync_locks WHERE workdir = ?1",
        params![workdir],
        |row| row.get(0),
    )?;
    let intelligence_nodes: i64 = tx.query_row(
        "SELECT COUNT(*) FROM intelligence_nodes WHERE project_hash = ?1",
        params![hash],
        |row| row.get(0),
    )?;
    let operational_sessions: i64 = tx.query_row(
        "SELECT COUNT(*) FROM operational_sessions WHERE project_hash = ?1",
        params![hash],
        |row| row.get(0),
    )?;

    // Follow-on (FK CASCADE) targets: only count rows attached to *this*
    // project's loops / interactive_sessions / intelligence_nodes.
    // Counting everything in the table would over-report by
    // attributing other projects' rows to this one.
    //
    // `loop_specs` (and everything keyed off it below) counts only
    // *loop-bound* specs (`loop_id` set, pointing at one of this
    // project's loops) — a standalone spec (`loop_id IS NULL`) is a
    // shared/backlog entity the cascade never deletes, even when its
    // own `workdir` column happens to match this project, so it must
    // never be counted here either (this count has to match exactly
    // what `cascade_delete_orphan_project` removes).
    //
    // Every placeholder below is `?1` reused, never `?2`/`?3` — passing
    // more than one bound value per query is a rusqlite parameter-count
    // mismatch, not "extra safety".
    let loop_specs: i64 = tx.query_row(
        "SELECT COUNT(*) FROM loop_specs
              WHERE loop_id IN (SELECT id FROM loops WHERE workdir = ?1)",
        params![workdir],
        |row| row.get(0),
    )?;
    let loop_nodes: i64 = tx.query_row(
            "SELECT COUNT(*) FROM loop_nodes ln
              WHERE (ln.loop_id IS NOT NULL AND ln.loop_id IN (SELECT id FROM loops WHERE workdir = ?1))
                 OR (ln.spec_id IS NOT NULL AND ln.spec_id IN (
                      SELECT id FROM loop_specs WHERE loop_id IN (SELECT id FROM loops WHERE workdir = ?1)
                 ))",
            params![workdir],
            |row| row.get(0),
        )?;
    let loop_edges: i64 = tx.query_row(
            "SELECT COUNT(*) FROM loop_edges le
              WHERE (le.loop_id IS NOT NULL AND le.loop_id IN (SELECT id FROM loops WHERE workdir = ?1))
                 OR (le.spec_id IS NOT NULL AND le.spec_id IN (
                      SELECT id FROM loop_specs WHERE loop_id IN (SELECT id FROM loops WHERE workdir = ?1)
                 ))",
            params![workdir],
            |row| row.get(0),
        )?;
    let loop_runs: i64 = tx.query_row(
        "SELECT COUNT(*) FROM loop_runs
              WHERE loop_id IN (SELECT id FROM loops WHERE workdir = ?1)",
        params![workdir],
        |row| row.get(0),
    )?;
    let loop_completion_hook_runs: i64 = tx.query_row(
        "SELECT COUNT(*) FROM loop_completion_hook_runs
              WHERE loop_id IN (SELECT id FROM loops WHERE workdir = ?1)",
        params![workdir],
        |row| row.get(0),
    )?;
    let ensembles: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ensembles
              WHERE (loop_id IS NOT NULL AND loop_id IN (SELECT id FROM loops WHERE workdir = ?1))
                 OR (spec_id IS NOT NULL AND spec_id IN (
                      SELECT id FROM loop_specs WHERE loop_id IN (SELECT id FROM loops WHERE workdir = ?1)
                 ))",
            params![workdir],
            |row| row.get(0),
        )?;
    let ensemble_members: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ensemble_members
              WHERE node_id IN (
                  SELECT id FROM loop_nodes ln
                   WHERE (ln.loop_id IS NOT NULL AND ln.loop_id IN (SELECT id FROM loops WHERE workdir = ?1))
                      OR (ln.spec_id IS NOT NULL AND ln.spec_id IN (
                           SELECT id FROM loop_specs WHERE loop_id IN (SELECT id FROM loops WHERE workdir = ?1)
                      ))
              )",
            params![workdir],
            |row| row.get(0),
        )?;
    let queue_members: i64 = tx.query_row(
            "SELECT COUNT(*) FROM queue_members
              WHERE spec_id IN (
                  SELECT id FROM loop_specs WHERE loop_id IN (SELECT id FROM loops WHERE workdir = ?1)
              )",
            params![workdir],
            |row| row.get(0),
        )?;
    let seed_sessions: i64 = tx.query_row(
        "SELECT COUNT(*) FROM seed_sessions
              WHERE session_id IN (SELECT id FROM interactive_sessions WHERE working_dir = ?1)",
        params![workdir],
        |row| row.get(0),
    )?;
    let intelligence_edges: i64 = tx.query_row(
        "SELECT COUNT(*) FROM intelligence_edges
              WHERE from_node_id IN (SELECT id FROM intelligence_nodes WHERE project_hash = ?1)
                 OR to_node_id   IN (SELECT id FROM intelligence_nodes WHERE project_hash = ?1)",
        params![hash],
        |row| row.get(0),
    )?;

    Ok(HardCascadeCounts {
        loops,
        interactive_sessions,
        terminal_sessions,
        last_prompts,
        scheduled_sends,
        failed_scheduled_sends,
        sync_messages,
        sync_locks,
        intelligence_nodes,
        operational_sessions,
        loop_specs,
        loop_nodes,
        loop_edges,
        loop_runs,
        loop_completion_hook_runs,
        ensembles,
        ensemble_members,
        queue_members,
        seed_sessions,
        intelligence_edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::loops::{LoopSpecStatus, LoopStatus};
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        // Leak the tempdir so the backing file survives for the DB's lifetime
        // within the test (mirrors the pattern used elsewhere in this crate).
        let path = dir.path().join("test.db");
        std::mem::forget(dir);
        Database::new(&path).unwrap()
    }

    #[test]
    fn list_cleanable_interactive_sessions_excludes_active_and_resumed() {
        let db = test_db();
        db.insert_interactive_session(
            "s-active",
            "active",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_interactive_session(
            "s-resumed",
            "resumed",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_interactive_session(
            "s-completed",
            "completed",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        // Force the statuses directly since insert always starts 'active'.
        db.mark_session_resumed("s-resumed").unwrap();
        db.finish_interactive_session("s-completed", 0).unwrap();

        let rows = db.list_cleanable_interactive_sessions().unwrap();
        let ids: HashSet<&str> = rows.iter().map(|s| s.id.as_str()).collect();
        assert!(!ids.contains("s-active"));
        assert!(!ids.contains("s-resumed"));
        assert!(ids.contains("s-completed"));
    }

    #[test]
    fn delete_interactive_sessions_is_transactional_batch() {
        let db = test_db();
        db.insert_interactive_session(
            "s1",
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
            "s2",
            "s2",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s1", 0).unwrap();
        db.finish_interactive_session("s2", 1).unwrap();

        let deleted = db
            .delete_interactive_sessions(&["s1".to_string(), "s2".to_string()])
            .unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(db.count_interactive_sessions().unwrap(), 0);
    }

    #[test]
    fn project_dependent_counts_reflect_workdir_scoped_rows() {
        let db = test_db();
        db.insert_interactive_session(
            "s1",
            "s1",
            "opencode",
            "/proj",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_terminal_session("t1", "t1", "bash", "/proj")
            .unwrap();

        let counts = db.project_dependent_counts("/proj").unwrap();
        assert_eq!(counts.interactive_sessions, 1);
        assert_eq!(counts.terminal_sessions, 1);
        assert_eq!(counts.loops, 0);

        let counts_other = db.project_dependent_counts("/elsewhere").unwrap();
        assert_eq!(counts_other.interactive_sessions, 0);
    }

    #[test]
    fn list_agent_ids_and_terminal_names_round_trip() {
        let db = test_db();
        assert!(db.list_agent_ids().unwrap().is_empty());
        db.insert_terminal_session("t1", "my-term", "bash", "/tmp")
            .unwrap();
        let names = db.list_terminal_session_names().unwrap();
        assert!(names.contains("my-term"));
    }

    fn make_project(hash: &str, path: &str) -> crate::domain::project::Project {
        crate::domain::project::Project {
            hash: hash.to_string(),
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            description: None,
            tags: None,
            indexed_at: None,
            created_at: 1_700_000_000,
        }
    }

    fn make_loop(
        id: &str,
        workdir: &str,
        status: crate::domain::loops::LoopStatus,
    ) -> crate::domain::loops::Loop {
        crate::domain::loops::Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
            id: id.to_string(),
            name: format!("loop-{id}"),
            description: None,
            workdir: workdir.to_string(),
            status,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        }
    }

    #[test]
    fn hard_cascade_counts_sums_direct_and_follow_on_rows() {
        let db = test_db();
        let workdir = "/proj-x";
        db.upsert_project(&make_project("hash-x", workdir)).unwrap();

        // Direct targets.
        db.insert_loop(&make_loop("loop-1", workdir, LoopStatus::Completed))
            .unwrap();
        db.insert_interactive_session(
            "s-old",
            "s-old",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-old", 0).unwrap();
        db.insert_terminal_session("t1", "t1", "bash", workdir)
            .unwrap();
        db.upsert_intelligence_node(crate::db::intelligence::IntelligenceNodeInput {
            id: None,
            kind: "fact".to_string(),
            status: None,
            title: "x".to_string(),
            body: "y".to_string(),
            metadata: None,
            project_hash: Some("hash-x".to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();

        // Cascade children attached to the loop.
        let spec = crate::domain::loops::LoopSpec {
            id: "spec-1".to_string(),
            loop_id: Some("loop-1".to_string()),
            name: "spec".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: Some(workdir.to_string()),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&spec).unwrap();
        let node = crate::domain::loops::LoopNode {
            id: "node-1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "n1".to_string(),
            kind: crate::domain::loops::LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 0,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&node).unwrap();

        // A standalone spec (no owning loop) whose own `workdir` happens to
        // match this project. It's a shared/backlog entity the cascade
        // never deletes (spec C2: "standalone specs ... are NOT deleted"),
        // so it must not inflate the printed plan either.
        db.insert_loop_spec(&crate::domain::loops::LoopSpec {
            id: "spec-standalone".to_string(),
            loop_id: None,
            name: "standalone".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: Some(workdir.to_string()),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();

        let counts = db.project_hard_cascade_counts("hash-x", workdir).unwrap();
        assert_eq!(counts.loops, 1);
        assert_eq!(counts.interactive_sessions, 1);
        assert_eq!(counts.terminal_sessions, 1);
        // `upsert_project` auto-creates a kind='project' intelligence root
        // node, so this is that root plus the fact node inserted above.
        assert_eq!(counts.intelligence_nodes, 2);
        // Only the loop-bound spec is counted; the standalone spec is not.
        assert_eq!(counts.loop_specs, 1);
        assert_eq!(counts.loop_nodes, 1);
    }

    #[test]
    fn hard_cascade_counts_zero_for_unknown_workdir() {
        let db = test_db();
        let counts = db
            .project_hard_cascade_counts("hash-never", "/never")
            .unwrap();
        assert!(counts.is_empty());
    }

    #[test]
    fn hard_cascade_skip_reason_detects_running_loop() {
        let db = test_db();
        let workdir = "/proj-running";
        db.upsert_project(&make_project("hash-run", workdir))
            .unwrap();
        db.insert_loop(&make_loop("loop-r", workdir, LoopStatus::Running))
            .unwrap();
        let reason = db.project_hard_cascade_skip_reason(workdir).unwrap();
        assert_eq!(reason, Some(HardCascadeSkipReason::RunningLoop));
    }

    #[test]
    fn hard_cascade_skip_reason_detects_active_session() {
        let db = test_db();
        let workdir = "/proj-active";
        db.upsert_project(&make_project("hash-act", workdir))
            .unwrap();
        db.insert_interactive_session(
            "s-live",
            "s-live",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        let reason = db.project_hard_cascade_skip_reason(workdir).unwrap();
        assert_eq!(reason, Some(HardCascadeSkipReason::ActiveSession));
    }

    #[test]
    fn hard_cascade_skip_reason_prefers_active_session_over_running_loop() {
        let db = test_db();
        let workdir = "/proj-both";
        db.upsert_project(&make_project("hash-both", workdir))
            .unwrap();
        db.insert_loop(&make_loop("loop-b", workdir, LoopStatus::Running))
            .unwrap();
        db.insert_interactive_session(
            "s-both",
            "s-both",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        let reason = db.project_hard_cascade_skip_reason(workdir).unwrap();
        assert_eq!(reason, Some(HardCascadeSkipReason::ActiveSession));
    }

    #[test]
    fn hard_cascade_skip_reason_none_for_completed_state() {
        let db = test_db();
        let workdir = "/proj-clean";
        db.upsert_project(&make_project("hash-clean", workdir))
            .unwrap();
        db.insert_loop(&make_loop("loop-c", workdir, LoopStatus::Completed))
            .unwrap();
        db.insert_interactive_session(
            "s-finished",
            "s-finished",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-finished", 0).unwrap();
        let reason = db.project_hard_cascade_skip_reason(workdir).unwrap();
        assert_eq!(reason, None);
    }

    #[test]
    fn cascade_delete_orphan_project_removes_all_dependents() {
        let db = test_db();
        let workdir = "/proj-cascade";
        let hash = "hash-cascade";
        db.upsert_project(&make_project(hash, workdir)).unwrap();
        db.insert_loop(&make_loop("loop-c", workdir, LoopStatus::Completed))
            .unwrap();
        let spec = crate::domain::loops::LoopSpec {
            id: "spec-c".to_string(),
            loop_id: Some("loop-c".to_string()),
            name: "spec".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: Some(workdir.to_string()),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&spec).unwrap();
        let node = crate::domain::loops::LoopNode {
            id: "node-c".to_string(),
            spec_id: Some("spec-c".to_string()),
            loop_id: None,
            name: "n1".to_string(),
            kind: crate::domain::loops::LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 0,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&node).unwrap();
        db.insert_interactive_session(
            "s-c",
            "s-c",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-c", 0).unwrap();
        db.insert_terminal_session("t-c", "t-c", "bash", workdir)
            .unwrap();
        db.upsert_intelligence_node(crate::db::intelligence::IntelligenceNodeInput {
            id: None,
            kind: "fact".to_string(),
            status: None,
            title: "c".to_string(),
            body: "d".to_string(),
            metadata: None,
            project_hash: Some(hash.to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();

        let counts = db.cascade_delete_orphan_project(hash, workdir).unwrap();
        assert_eq!(counts.loops, 1);
        assert_eq!(counts.interactive_sessions, 1);
        assert_eq!(counts.terminal_sessions, 1);
        // The project-root node `upsert_project` auto-creates, plus the
        // fact node inserted above.
        assert_eq!(counts.intelligence_nodes, 2);
        // Removed only via the loop's FK CASCADE (no explicit DELETE
        // touches loop_specs/loop_nodes) — asserting the exact count here
        // is what catches a returned-count regression: an explicit
        // post-cascade "sweep" DELETE always affects zero rows once the
        // parent DELETE already cascaded the row away, which would
        // silently report 0 instead of 1.
        assert_eq!(counts.loop_specs, 1);
        assert_eq!(counts.loop_nodes, 1);

        // The project row and every dependent must be gone.
        assert!(db.get_project(hash).unwrap().is_none());
        assert_eq!(db.project_dependent_counts(workdir).unwrap().loops, 0);
        assert_eq!(
            db.project_dependent_counts(workdir)
                .unwrap()
                .interactive_sessions,
            0
        );
        assert_eq!(
            db.project_dependent_counts(workdir)
                .unwrap()
                .terminal_sessions,
            0
        );
        // Spec was loop-bound, so it should be CASCADE-deleted with the loop.
        assert!(db.get_loop_spec("spec-c").unwrap().is_none());
        assert!(db.get_loop_node("node-c").unwrap().is_none());
    }

    #[test]
    fn cascade_delete_orphan_project_leaves_unrelated_projects_intact() {
        let db = test_db();
        db.upsert_project(&make_project("hash-a", "/proj-a"))
            .unwrap();
        db.upsert_project(&make_project("hash-b", "/proj-b"))
            .unwrap();
        db.insert_loop(&make_loop("loop-a", "/proj-a", LoopStatus::Completed))
            .unwrap();
        db.insert_loop(&make_loop("loop-b", "/proj-b", LoopStatus::Completed))
            .unwrap();

        db.cascade_delete_orphan_project("hash-a", "/proj-a")
            .unwrap();

        assert!(db.get_project("hash-a").unwrap().is_none());
        assert!(db.get_project("hash-b").unwrap().is_some());
        assert!(db.get_loop("loop-b").unwrap().is_some());
    }

    #[test]
    fn parse_rfc3339_ts_valid_timestamp() {
        assert_eq!(parse_rfc3339_ts("2024-01-15T10:30:00Z"), 1705314600);
    }

    #[test]
    fn parse_rfc3339_ts_valid_with_offset() {
        // 2024-01-15T10:30:00+05:00 == 2024-01-15T05:30:00Z == 1705296600
        assert_eq!(parse_rfc3339_ts("2024-01-15T10:30:00+05:00"), 1705296600);
    }

    #[test]
    fn parse_rfc3339_ts_invalid_returns_zero() {
        assert_eq!(parse_rfc3339_ts("not-a-timestamp"), 0);
    }

    #[test]
    fn parse_rfc3339_ts_empty_string_returns_zero() {
        assert_eq!(parse_rfc3339_ts(""), 0);
    }

    #[test]
    fn parse_rfc3339_ts_partial_date_returns_zero() {
        assert_eq!(parse_rfc3339_ts("2024-01-15"), 0);
    }

    #[test]
    fn parse_rfc3339_ts_epoch() {
        assert_eq!(parse_rfc3339_ts("1970-01-01T00:00:00Z"), 0);
    }

    #[test]
    fn parse_rfc3339_ts_negative_epoch() {
        assert_eq!(parse_rfc3339_ts("1969-12-31T23:59:59Z"), -1);
    }

    #[test]
    fn delete_interactive_sessions_empty_vec_returns_zero() {
        let db = test_db();
        let deleted = db.delete_interactive_sessions(&[]).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn hard_cascade_skip_reason_describe_texts() {
        assert_eq!(
            HardCascadeSkipReason::RunningLoop.describe(),
            "has a running loop"
        );
        assert_eq!(
            HardCascadeSkipReason::ActiveSession.describe(),
            "has an active/resumed interactive session"
        );
    }

    #[test]
    fn hard_cascade_counts_is_empty_true_when_all_zero() {
        let counts = HardCascadeCounts::default();
        assert!(counts.is_empty());
    }

    #[test]
    fn hard_cascade_counts_is_empty_false_when_any_nonzero() {
        let counts = HardCascadeCounts {
            ensembles: 1,
            ..HardCascadeCounts::default()
        };
        assert!(!counts.is_empty());
    }

    #[test]
    fn hard_cascade_counts_is_empty_false_for_each_field() {
        let fields = [
            "loops",
            "interactive_sessions",
            "terminal_sessions",
            "last_prompts",
            "scheduled_sends",
            "failed_scheduled_sends",
            "sync_messages",
            "sync_locks",
            "intelligence_nodes",
            "loop_specs",
            "loop_nodes",
            "loop_edges",
            "loop_runs",
            "loop_completion_hook_runs",
            "ensembles",
            "ensemble_members",
            "queue_members",
            "seed_sessions",
            "intelligence_edges",
        ];
        for field in &fields {
            let mut counts = HardCascadeCounts::default();
            // Set each field to 1 individually
            match *field {
                "loops" => counts.loops = 1,
                "interactive_sessions" => counts.interactive_sessions = 1,
                "terminal_sessions" => counts.terminal_sessions = 1,
                "last_prompts" => counts.last_prompts = 1,
                "scheduled_sends" => counts.scheduled_sends = 1,
                "failed_scheduled_sends" => counts.failed_scheduled_sends = 1,
                "sync_messages" => counts.sync_messages = 1,
                "sync_locks" => counts.sync_locks = 1,
                "intelligence_nodes" => counts.intelligence_nodes = 1,
                "loop_specs" => counts.loop_specs = 1,
                "loop_nodes" => counts.loop_nodes = 1,
                "loop_edges" => counts.loop_edges = 1,
                "loop_runs" => counts.loop_runs = 1,
                "loop_completion_hook_runs" => counts.loop_completion_hook_runs = 1,
                "ensembles" => counts.ensembles = 1,
                "ensemble_members" => counts.ensemble_members = 1,
                "queue_members" => counts.queue_members = 1,
                "seed_sessions" => counts.seed_sessions = 1,
                "intelligence_edges" => counts.intelligence_edges = 1,
                _ => unreachable!(),
            }
            assert!(
                !counts.is_empty(),
                "is_empty should be false when {field} = 1"
            );
        }
    }

    #[test]
    fn list_cleanable_interactive_sessions_includes_orphaned_and_error() {
        let db = test_db();
        // Insert as active, then mark orphaned
        db.insert_interactive_session(
            "s-orphaned",
            "s-orphaned",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.mark_session_orphaned("s-orphaned").unwrap();

        // Insert as active, then finish with error (non-zero exit code)
        db.insert_interactive_session(
            "s-error",
            "s-error",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-error", 1).unwrap();

        let rows = db.list_cleanable_interactive_sessions().unwrap();
        let ids: HashSet<&str> = rows.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains("s-orphaned"));
        assert!(ids.contains("s-error"));
    }

    #[test]
    fn list_cleanable_interactive_sessions_empty_db() {
        let db = test_db();
        let rows = db.list_cleanable_interactive_sessions().unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn project_dependent_counts_empty_workdir() {
        let db = test_db();
        let counts = db.project_dependent_counts("").unwrap();
        assert_eq!(counts.loops, 0);
        assert_eq!(counts.interactive_sessions, 0);
        assert_eq!(counts.terminal_sessions, 0);
    }

    #[test]
    fn cascade_delete_orphan_project_preserves_other_projects_queue_members() {
        let db = test_db();
        let workdir = "/proj-queue";
        db.upsert_project(&make_project("hash-p", workdir)).unwrap();
        db.insert_loop(&make_loop("loop-p", workdir, LoopStatus::Completed))
            .unwrap();
        let doomed_spec_id = "spec-p-1";
        let safe_spec_id = "spec-p-2";
        db.insert_loop_spec(&crate::domain::loops::LoopSpec {
            id: doomed_spec_id.to_string(),
            loop_id: Some("loop-p".to_string()),
            name: "d".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: Some(workdir.to_string()),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        db.insert_loop_spec(&crate::domain::loops::LoopSpec {
            id: safe_spec_id.to_string(),
            loop_id: None,
            name: "s".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: Some("/elsewhere".to_string()),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        let queue = crate::domain::queues::Queue {
            id: "queue-1".to_string(),
            name: "p1".to_string(),
            created_at: chrono::Utc::now(),
        };
        db.insert_queue(&queue).unwrap();
        db.append_queue_member(&queue.id, doomed_spec_id, None)
            .unwrap();
        db.append_queue_member(&queue.id, safe_spec_id, None)
            .unwrap();

        db.cascade_delete_orphan_project("hash-p", workdir).unwrap();

        // Queue itself remains (shared, not project-owned).
        assert!(db.get_queue(&queue.id).unwrap().is_some());
        // The doomed spec is gone (loop-bound → CASCADE).
        assert!(db.get_loop_spec(doomed_spec_id).unwrap().is_none());
        // The standalone spec survives, and so does its queue membership.
        assert!(db.get_loop_spec(safe_spec_id).unwrap().is_some());
        assert!(db.queue_has_member(&queue.id, safe_spec_id).unwrap());
        // The queue membership that pointed at the doomed spec is gone.
        assert!(!db.queue_has_member(&queue.id, doomed_spec_id).unwrap());
    }

    #[test]
    fn list_cleanable_interactive_sessions_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let sessions = db.list_cleanable_interactive_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn delete_interactive_sessions_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let deleted = db.delete_interactive_sessions(&[]).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn list_agent_ids_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let ids = db.list_agent_ids().unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn reclaim_space_shrinks_file_after_bulk_delete() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::new(&path).unwrap();

        // Pad each row so the bulk insert actually grows the file across
        // multiple pages instead of fitting in whatever SQLite pre-allocates.
        let padding = "x".repeat(4096);
        let mut ids = Vec::new();
        for i in 0..500 {
            let id = format!("s-{i}");
            db.insert_interactive_session(
                &id,
                &id,
                "opencode",
                "/tmp",
                Some(&padding),
                None,
                "interactive",
                None,
            )
            .unwrap();
            db.finish_interactive_session(&id, 0).unwrap();
            ids.push(id);
        }

        // Force everything out of the WAL and into the main file so the
        // "before" measurement reflects real page count, not whatever's
        // still sitting in `-wal`.
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(FULL);").unwrap();
        }
        let size_before_delete = std::fs::metadata(&path).unwrap().len();

        db.delete_interactive_sessions(&ids).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(FULL);").unwrap();
        }
        let size_after_delete = std::fs::metadata(&path).unwrap().len();
        // Deleting rows frees pages inside the file without shrinking it —
        // this is the premise `reclaim_space` exists to fix.
        assert_eq!(size_after_delete, size_before_delete);

        db.reclaim_space().unwrap();
        let size_after_reclaim = std::fs::metadata(&path).unwrap().len();
        assert!(
            size_after_reclaim < size_after_delete,
            "expected reclaim to shrink the file: {size_after_delete} -> {size_after_reclaim}"
        );

        // The database must still be fully usable afterwards.
        db.insert_interactive_session(
            "s-post-vacuum",
            "s-post-vacuum",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        assert_eq!(db.count_interactive_sessions().unwrap(), 1);
    }

    #[test]
    fn list_terminal_session_names_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let names = db.list_terminal_session_names().unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn project_dependent_counts_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let counts = db.project_dependent_counts("/nonexistent").unwrap();
        assert_eq!(counts.loops, 0);
        assert_eq!(counts.interactive_sessions, 0);
        assert_eq!(counts.terminal_sessions, 0);
    }

    #[test]
    fn parse_rfc3339_ts_valid() {
        let ts = parse_rfc3339_ts("2024-01-15T10:30:00Z");
        assert!(ts > 0);
    }

    #[test]
    fn parse_rfc3339_ts_invalid() {
        let ts = parse_rfc3339_ts("invalid");
        assert_eq!(ts, 0);
    }

    // ── quick_check ─────────────────────────────────────────────────────

    #[test]
    fn quick_check_reports_ok_for_a_healthy_database() {
        let db = test_db();
        assert_eq!(db.quick_check().unwrap(), "ok");
    }

    #[test]
    fn quick_check_reports_the_problem_for_a_corrupted_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.db");
        // Build a real database first so the file has a valid SQLite header,
        // then stomp on page data after every handle to it is dropped —
        // writing garbage over a file that was never a database at all just
        // produces a "not a database" file-format error, not the kind of
        // in-page corruption `quick_check` is meant to catch.
        {
            let db = Database::new(&path).unwrap();
            db.insert_terminal_session("t1", "t1", "bash", "/tmp")
                .unwrap();
            drop(db);
        }
        let mut bytes = std::fs::read(&path).unwrap();
        // CM5: a fixed offset in page 3 rather than bytes.len()/2 — the
        // subagent_runs table shifted the file so the midpoint no longer
        // lands in a page quick_check validates.
        let start = 8192;
        let end = (start + 100).min(bytes.len());
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }
        std::fs::write(&path, &bytes).unwrap();

        let db = Database::new(&path).unwrap();
        let verdict = db.quick_check().unwrap();
        assert_ne!(verdict, "ok", "corrupted file must not report ok");
    }

    // ── busy_reasons ────────────────────────────────────────────────────

    #[test]
    fn busy_reasons_empty_when_nothing_in_flight() {
        let db = test_db();
        assert!(db.busy_reasons().unwrap().is_empty());
    }

    #[test]
    fn busy_reasons_reports_a_running_loop() {
        let db = test_db();
        db.insert_loop(&make_loop("loop-1", "/tmp/proj", LoopStatus::Running))
            .unwrap();

        let reasons = db.busy_reasons().unwrap();
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("running"), "{reasons:?}");
    }

    #[test]
    fn busy_reasons_ignores_a_completed_loop() {
        let db = test_db();
        db.insert_loop(&make_loop("loop-1", "/tmp/proj", LoopStatus::Completed))
            .unwrap();

        assert!(db.busy_reasons().unwrap().is_empty());
    }

    #[test]
    fn busy_reasons_reports_an_active_interactive_session() {
        let db = test_db();
        db.insert_interactive_session(
            "s-active",
            "s-active",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();

        let reasons = db.busy_reasons().unwrap();
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("interactive session"), "{reasons:?}");
    }

    #[test]
    fn busy_reasons_ignores_a_completed_session() {
        let db = test_db();
        db.insert_interactive_session(
            "s-done",
            "s-done",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-done", 0).unwrap();

        assert!(db.busy_reasons().unwrap().is_empty());
    }
}
