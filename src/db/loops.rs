use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::io::{Error as IoError, ErrorKind};

use crate::db::Database;
use crate::domain::loops::{
    Loop, LoopDetails, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind, LoopNodeRun,
    LoopRunStatus, LoopSpec, LoopSpecDetails, LoopSpecStatus, LoopStatus, SpecPool,
};
use crate::domain::models::Trigger;

impl Database {
    pub fn delete_loop(&self, loop_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute("DELETE FROM loops WHERE id = ?1", params![loop_id])?;
        Ok(())
    }

    pub fn insert_loop(&self, lp: &Loop) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let (trigger_type, trigger_config) = encode_loop_trigger(lp.trigger.as_ref())?;
        let spec_pool = lp
            .spec_pool
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        conn.execute(
            "INSERT INTO loops (id, name, description, workdir, status, trigger_type, trigger_config, created_at, started_at, completed_at, autorun_at, spec_pool)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                &lp.id,
                &lp.name,
                &lp.description,
                &lp.workdir,
                lp.status.as_str(),
                trigger_type,
                trigger_config,
                lp.created_at.timestamp(),
                lp.started_at.map(|value| value.timestamp()),
                lp.completed_at.map(|value| value.timestamp()),
                lp.autorun_at.map(|value| value.timestamp()),
                spec_pool,
            ],
        )?;
        Ok(())
    }

    /// Schedule a one-shot resume for a loop at `at`. The scheduler fires it
    /// once `at` is reached (if the loop is fireable) and clears the field —
    /// see [`crate::domain::loops::Loop::is_autorun_due`].
    pub fn schedule_loop_autorun(&self, loop_id: &str, at: DateTime<Utc>) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loops SET autorun_at = ?1 WHERE id = ?2",
            params![at.timestamp(), loop_id],
        )?;
        Ok(rows > 0)
    }

    /// Clear a loop's pending one-shot autorun schedule, without touching status.
    pub fn clear_loop_autorun(&self, loop_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loops SET autorun_at = NULL WHERE id = ?1",
            params![loop_id],
        )?;
        Ok(rows > 0)
    }

    /// Loops with a pending one-shot autorun schedule (regardless of trigger).
    pub fn list_pending_autorun_loops(&self) -> Result<Vec<Loop>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, spec_pool
             FROM loops WHERE autorun_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([], map_loop_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Replace a loop's trigger (cron/watch/manual). Passing `None` clears any
    /// existing trigger, making the loop manual-only.
    pub fn update_loop_trigger(&self, loop_id: &str, trigger: Option<&Trigger>) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let (trigger_type, trigger_config) = encode_loop_trigger(trigger)?;
        let rows = conn.execute(
            "UPDATE loops SET trigger_type = ?1, trigger_config = ?2 WHERE id = ?3",
            params![trigger_type, trigger_config, loop_id],
        )?;
        Ok(rows > 0)
    }

    /// Loops that fire on a cron schedule (their trigger is `Cron`).
    pub fn list_cron_loops(&self) -> Result<Vec<Loop>> {
        self.list_loops_where_trigger("cron")
    }

    /// Loops that fire on a file-system watch (their trigger is `Watch`).
    pub fn list_watch_loops(&self) -> Result<Vec<Loop>> {
        self.list_loops_where_trigger("watch")
    }

    fn list_loops_where_trigger(&self, trigger_type: &str) -> Result<Vec<Loop>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, spec_pool
             FROM loops WHERE trigger_type = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![trigger_type], map_loop_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_loop_details(
        &self,
        loop_id: &str,
        name: Option<&str>,
        description: Option<Option<&str>>,
        workdir: Option<&str>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loops
             SET name = COALESCE(?1, name),
                 description = CASE
                     WHEN ?2 IS NULL THEN description
                     ELSE ?3
                 END,
                 workdir = COALESCE(?4, workdir)
             WHERE id = ?5",
            params![
                name,
                description.map(|_| 1),
                description.flatten(),
                workdir,
                loop_id
            ],
        )?;
        Ok(rows > 0)
    }

    pub fn get_loop(&self, loop_id: &str) -> Result<Option<Loop>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, spec_pool
             FROM loops WHERE id = ?1",
        )?;

        stmt.query_row(params![loop_id], map_loop_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn list_loops(&self, workdir: Option<&str>) -> Result<Vec<Loop>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let sql = if workdir.is_some() {
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, spec_pool
             FROM loops WHERE workdir = ?1 ORDER BY created_at DESC"
        } else {
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, spec_pool
             FROM loops ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(workdir) = workdir {
            stmt.query_map(params![workdir], map_loop_row)?
        } else {
            stmt.query_map([], map_loop_row)?
        };

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_loop_status(
        &self,
        loop_id: &str,
        status: LoopStatus,
        started_at: Option<DateTime<Utc>>,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loops
             SET status = ?1,
                 started_at = COALESCE(?2, started_at),
                 completed_at = COALESCE(?3, completed_at)
             WHERE id = ?4",
            params![
                status.as_str(),
                started_at.map(|value| value.timestamp()),
                completed_at.map(|value| value.timestamp()),
                loop_id,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Reset a loop back to `Draft` (the status `loop_run` accepts) and clear
    /// `completed_at`, so a `failed` or `completed` loop can be relaunched via
    /// `loop_reset` + `loop_run` instead of being stuck forever.
    pub fn reset_loop_status(&self, loop_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loops SET status = ?1, completed_at = NULL WHERE id = ?2",
            params![LoopStatus::Draft.as_str(), loop_id],
        )?;
        Ok(rows > 0)
    }

    /// Reset a loop spec back to `Pending`, clearing `started_at` and
    /// `completed_at` unconditionally (unlike [`Self::update_loop_spec_status`],
    /// which only overwrites when a new value is given). Used by `loop_reset`.
    pub fn reset_loop_spec_status(&self, spec_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_specs SET status = ?1, started_at = NULL, completed_at = NULL, spec_start_head = NULL WHERE id = ?2",
            params![LoopSpecStatus::Pending.as_str(), spec_id],
        )?;
        Ok(rows > 0)
    }

    /// Replace a loop's `spec_pool` with `pool` (serialized as JSON).
    pub fn update_loop_spec_pool(&self, loop_id: &str, pool: &SpecPool) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let encoded = serde_json::to_string(pool)?;
        let rows = conn.execute(
            "UPDATE loops SET spec_pool = ?1 WHERE id = ?2",
            params![encoded, loop_id],
        )?;
        Ok(rows > 0)
    }

    pub fn insert_loop_spec(&self, spec: &LoopSpec) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_specs (id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &spec.id,
                &spec.loop_id,
                &spec.name,
                &spec.description,
                spec.position,
                spec.parallelizable,
                spec.status.as_str(),
                spec.started_at.map(|value| value.timestamp()),
                spec.completed_at.map(|value| value.timestamp()),
                &spec.spec_start_head,
            ],
        )?;
        Ok(())
    }

    pub fn list_loop_specs(&self, loop_id: &str) -> Result<Vec<LoopSpec>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head
             FROM loop_specs WHERE loop_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![loop_id], map_loop_spec_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_loop_spec(&self, spec_id: &str) -> Result<Option<LoopSpec>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head
             FROM loop_specs WHERE id = ?1",
        )?;

        stmt.query_row(params![spec_id], map_loop_spec_row)
            .optional()
            .map_err(Into::into)
    }

    /// Record the workdir's git HEAD at the moment a spec starts running.
    /// Called once per spec (not per node) — see [`crate::loop_engine`]'s
    /// `{{spec_start_head}}` placeholder. `head = None` means the workdir
    /// isn't a git repo; the column is cleared rather than left stale.
    pub fn set_loop_spec_start_head(&self, spec_id: &str, head: Option<&str>) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_specs SET spec_start_head = ?1 WHERE id = ?2",
            params![head, spec_id],
        )?;
        Ok(rows > 0)
    }

    pub fn update_loop_spec_details(
        &self,
        spec_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        position: Option<i64>,
        parallelizable: Option<bool>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_specs
             SET name = COALESCE(?1, name),
                 description = COALESCE(?2, description),
                 position = COALESCE(?3, position),
                 parallelizable = COALESCE(?4, parallelizable)
             WHERE id = ?5",
            params![name, description, position, parallelizable, spec_id],
        )?;
        Ok(rows > 0)
    }

    pub fn update_loop_spec_status(
        &self,
        spec_id: &str,
        status: LoopSpecStatus,
        started_at: Option<DateTime<Utc>>,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_specs
             SET status = ?1,
                 started_at = COALESCE(?2, started_at),
                 completed_at = COALESCE(?3, completed_at)
             WHERE id = ?4",
            params![
                status.as_str(),
                started_at.map(|value| value.timestamp()),
                completed_at.map(|value| value.timestamp()),
                spec_id,
            ],
        )?;
        Ok(rows > 0)
    }

    pub fn insert_loop_node(&self, node: &LoopNode) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_nodes (id, spec_id, name, kind, config, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &node.id,
                &node.spec_id,
                &node.name,
                node.kind.as_str(),
                serde_json::to_string(&node.config)?,
                node.position,
                node.created_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn list_loop_nodes(&self, spec_id: &str) -> Result<Vec<LoopNode>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, name, kind, config, position, created_at
             FROM loop_nodes WHERE spec_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_loop_node_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_loop_node(&self, node_id: &str) -> Result<Option<LoopNode>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, name, kind, config, position, created_at
             FROM loop_nodes WHERE id = ?1",
        )?;
        stmt.query_row(params![node_id], map_loop_node_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn update_loop_node_details(
        &self,
        node_id: &str,
        name: Option<&str>,
        kind: Option<LoopNodeKind>,
        config: Option<&Value>,
        position: Option<i64>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_nodes
             SET name = COALESCE(?1, name),
                 kind = COALESCE(?2, kind),
                 config = COALESCE(?3, config),
                 position = COALESCE(?4, position)
             WHERE id = ?5",
            params![
                name,
                kind.map(|value| value.as_str()),
                config.map(serde_json::to_string).transpose()?,
                position,
                node_id,
            ],
        )?;
        Ok(rows > 0)
    }

    pub fn insert_loop_edge(&self, edge: &LoopEdge) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_edges (id, spec_id, from_node, to_node, condition)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &edge.id,
                &edge.spec_id,
                &edge.from_node,
                &edge.to_node,
                edge.condition.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn list_loop_edges(&self, spec_id: &str) -> Result<Vec<LoopEdge>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, from_node, to_node, condition
             FROM loop_edges WHERE spec_id = ?1 ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_loop_edge_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_loop_edge(&self, edge_id: &str) -> Result<Option<LoopEdge>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, from_node, to_node, condition
             FROM loop_edges WHERE id = ?1",
        )?;
        stmt.query_row(params![edge_id], map_loop_edge_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn update_loop_edge_condition(
        &self,
        edge_id: &str,
        condition: LoopEdgeCondition,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_edges
             SET condition = ?1
             WHERE id = ?2",
            params![condition.as_str(), edge_id],
        )?;
        Ok(rows > 0)
    }

    pub fn insert_loop_run(&self, run: &LoopNodeRun) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_runs (id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &run.id,
                &run.loop_id,
                &run.spec_id,
                &run.node_id,
                run.status.as_str(),
                run.input
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                run.output
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                run.started_at.timestamp(),
                run.completed_at.map(|value| value.timestamp()),
                run.iteration,
            ],
        )?;
        Ok(())
    }

    pub fn list_loop_runs_for_spec(&self, spec_id: &str) -> Result<Vec<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration
             FROM loop_runs WHERE spec_id = ?1 ORDER BY started_at ASC, iteration ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_loop_run_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_loop_run(&self, run_id: &str) -> Result<Option<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration
             FROM loop_runs WHERE id = ?1",
        )?;

        stmt.query_row(params![run_id], map_loop_run_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn get_active_loop_run_for_node(&self, node_id: &str) -> Result<Option<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration
             FROM loop_runs
             WHERE node_id = ?1 AND status = 'running'
             ORDER BY started_at DESC
             LIMIT 1",
        )?;

        stmt.query_row(params![node_id], map_loop_run_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn update_loop_run_result(
        &self,
        run_id: &str,
        status: LoopRunStatus,
        output: Option<&Value>,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_runs
             SET status = ?1,
                 output = COALESCE(?2, output),
                 completed_at = COALESCE(?3, completed_at)
             WHERE id = ?4",
            params![
                status.as_str(),
                output.map(serde_json::to_string).transpose()?,
                completed_at.map(|value| value.timestamp()),
                run_id,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Loops left `Running` when the daemon starts.
    pub fn list_running_loops(&self) -> Result<Vec<Loop>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, spec_pool
             FROM loops WHERE status = ?1",
        )?;
        let rows = stmt.query_map(params![LoopStatus::Running.as_str()], map_loop_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Node runs still `running` for a loop.
    fn running_loop_runs(&self, loop_id: &str) -> Result<Vec<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration
             FROM loop_runs WHERE loop_id = ?1 AND status = 'running'",
        )?;
        let rows = stmt.query_map(params![loop_id], map_loop_run_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Reconcile loops orphaned by a daemon restart.
    ///
    /// No loop run survives the process that spawned it, so any loop still
    /// `Running` at startup was interrupted mid-execution by the previous
    /// daemon. Pause it — its spec keeps its `Running` status so
    /// `resolve_spec_start` resumes at the same node — and mark its dangling
    /// node runs as failed/interrupted, so `loop_continue` alone is enough to
    /// resume it (no `loop_pause` detour needed). Idempotent: a loop already
    /// `Paused` isn't touched by a later call.
    pub fn reconcile_orphaned_loops(&self) -> Result<usize> {
        let orphaned = self.list_running_loops()?;
        for lp in &orphaned {
            let dangling_runs = self.running_loop_runs(&lp.id)?;
            if dangling_runs.is_empty() {
                tracing::warn!(
                    "Reconciling orphaned loop '{}': no active node run found; pausing.",
                    lp.id
                );
            }
            for run in &dangling_runs {
                tracing::warn!(
                    "Reconciling orphaned loop '{}': was running node '{}' when the daemon last stopped; pausing loop and marking its run as interrupted.",
                    lp.id,
                    run.node_id
                );
                self.update_loop_run_result(
                    &run.id,
                    LoopRunStatus::Fail,
                    Some(&serde_json::json!({
                        "interrupted": true,
                        "reason": "daemon restarted while this node was running"
                    })),
                    Some(Utc::now()),
                )?;
            }
            self.update_loop_status(&lp.id, LoopStatus::Paused, None, None)?;
        }
        Ok(orphaned.len())
    }

    pub fn get_loop_details(&self, loop_id: &str) -> Result<Option<LoopDetails>> {
        let Some(lp) = self.get_loop(loop_id)? else {
            return Ok(None);
        };
        let specs = self
            .list_loop_specs(loop_id)?
            .into_iter()
            .map(|spec| {
                let nodes = self.list_loop_nodes(&spec.id)?;
                let edges = self.list_loop_edges(&spec.id)?;
                Ok(LoopSpecDetails { spec, nodes, edges })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Some(LoopDetails { lp, specs }))
    }
}

fn map_loop_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Loop> {
    let trigger = row
        .get::<_, Option<String>>(5)?
        .as_deref()
        .map(decode_loop_trigger)
        .transpose()?;
    Ok(Loop {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        workdir: row.get(3)?,
        status: LoopStatus::from_str(&row.get::<_, String>(4)?),
        trigger,
        created_at: from_timestamp(row.get(6)?)?,
        started_at: row
            .get::<_, Option<i64>>(7)?
            .map(from_timestamp)
            .transpose()?,
        completed_at: row
            .get::<_, Option<i64>>(8)?
            .map(from_timestamp)
            .transpose()?,
        autorun_at: row
            .get::<_, Option<i64>>(9)?
            .map(from_timestamp)
            .transpose()?,
        spec_pool: row
            .get::<_, Option<String>>(10)?
            .as_deref()
            .map(decode_spec_pool)
            .transpose()?,
    })
}

/// Decode the `spec_pool` JSON column back into a [`SpecPool`].
fn decode_spec_pool(raw: &str) -> rusqlite::Result<SpecPool> {
    serde_json::from_str(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(error))
    })
}

/// Encode a loop trigger into the `(trigger_type, trigger_config)` column pair,
/// mirroring how agents persist their trigger: a short type label plus the full
/// trigger serialized as JSON.
fn encode_loop_trigger(trigger: Option<&Trigger>) -> Result<(Option<String>, Option<String>)> {
    match trigger {
        Some(trigger) => Ok((
            Some(trigger.type_str().to_string()),
            Some(serde_json::to_string(trigger)?),
        )),
        None => Ok((None, None)),
    }
}

/// Decode the `trigger_config` JSON back into a [`Trigger`].
fn decode_loop_trigger(raw: &str) -> rusqlite::Result<Trigger> {
    serde_json::from_str(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn map_loop_spec_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoopSpec> {
    Ok(LoopSpec {
        id: row.get(0)?,
        loop_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        position: row.get(4)?,
        parallelizable: row.get(5)?,
        status: LoopSpecStatus::from_str(&row.get::<_, String>(6)?),
        started_at: row
            .get::<_, Option<i64>>(7)?
            .map(from_timestamp)
            .transpose()?,
        completed_at: row
            .get::<_, Option<i64>>(8)?
            .map(from_timestamp)
            .transpose()?,
        spec_start_head: row.get(9)?,
    })
}

fn map_loop_node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoopNode> {
    let kind = LoopNodeKind::from_str(&row.get::<_, String>(3)?).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(IoError::new(
                ErrorKind::InvalidData,
                "Invalid loop node kind",
            )),
        )
    })?;
    let config_raw: String = row.get(4)?;
    let config = parse_json_value(&config_raw)?;

    Ok(LoopNode {
        id: row.get(0)?,
        spec_id: row.get(1)?,
        name: row.get(2)?,
        kind,
        config,
        position: row.get(5)?,
        created_at: from_timestamp(row.get(6)?)?,
    })
}

fn map_loop_edge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoopEdge> {
    let condition = LoopEdgeCondition::from_str(&row.get::<_, String>(4)?).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(IoError::new(
                ErrorKind::InvalidData,
                "Invalid loop edge condition",
            )),
        )
    })?;

    Ok(LoopEdge {
        id: row.get(0)?,
        spec_id: row.get(1)?,
        from_node: row.get(2)?,
        to_node: row.get(3)?,
        condition,
    })
}

fn map_loop_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoopNodeRun> {
    Ok(LoopNodeRun {
        id: row.get(0)?,
        loop_id: row.get(1)?,
        spec_id: row.get(2)?,
        node_id: row.get(3)?,
        status: LoopRunStatus::from_str(&row.get::<_, String>(4)?),
        input: row
            .get::<_, Option<String>>(5)?
            .as_deref()
            .map(parse_json_value)
            .transpose()?,
        output: row
            .get::<_, Option<String>>(6)?
            .as_deref()
            .map(parse_json_value)
            .transpose()?,
        started_at: from_timestamp(row.get(7)?)?,
        completed_at: row
            .get::<_, Option<i64>>(8)?
            .map(from_timestamp)
            .transpose()?,
        iteration: row.get(9)?,
    })
}

fn from_timestamp(value: i64) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(value, 0).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(IoError::new(
                ErrorKind::InvalidData,
                "Invalid timestamp value",
            )),
        )
    })
}

fn parse_json_value(raw: &str) -> rusqlite::Result<Value> {
    serde_json::from_str(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}
