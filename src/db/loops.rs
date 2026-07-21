use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::io::{Error as IoError, ErrorKind};

use crate::db::Database;
use crate::domain::loops::{
    Loop, LoopCompletionHook, LoopCompletionHookRun, LoopDetails, LoopEdge, LoopEdgeCondition,
    LoopNode, LoopNodeKind, LoopNodeRun, LoopResetOutcome, LoopRunStatus, LoopSpec,
    LoopSpecDetails, LoopSpecStatus, LoopStatus, SpecAdminStatusOutcome,
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
        let on_completed = encode_loop_completion_hook(lp.on_completed.as_ref())?;
        conn.execute(
            "INSERT INTO loops (id, name, description, workdir, status, trigger_type, trigger_config, created_at, started_at, completed_at, autorun_at, active_run_pool_id, on_completed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                &lp.active_run_pool_id,
                on_completed,
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
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, active_run_pool_id, on_completed
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

    /// Replace a loop's `on_completed` hook config (N2). Passing `None`
    /// clears it, making the loop's completion behave exactly as it did
    /// before N2 (no hook).
    pub fn update_loop_completion_hook(
        &self,
        loop_id: &str,
        hook: Option<&LoopCompletionHook>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let on_completed = encode_loop_completion_hook(hook)?;
        let rows = conn.execute(
            "UPDATE loops SET on_completed = ?1 WHERE id = ?2",
            params![on_completed, loop_id],
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
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, active_run_pool_id, on_completed
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
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, active_run_pool_id, on_completed
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
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, active_run_pool_id, on_completed
             FROM loops WHERE workdir = ?1 ORDER BY created_at DESC"
        } else {
            "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, active_run_pool_id, on_completed
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

    /// Atomically claim `loop_id` for a run by flipping it to `Running`, but
    /// only if it is not *already* `Running` (B42). Returns `true` when this
    /// call won the claim (the loop was fireable and is now `Running`), `false`
    /// when the loop was already `Running` — i.e. another dispatch is live and
    /// this launch must be treated as a no-op rather than starting a duplicate,
    /// superseding run.
    ///
    /// This is a single-statement compare-and-set, so two dispatches racing to
    /// launch the same loop (the classic autorun-vs-resume check-then-act race:
    /// one reads the loop as `failed`, the other hasn't written `running` yet)
    /// serialize on the connection lock and exactly one wins — the guard is on
    /// the status *transition* itself, not on a separate earlier read.
    pub fn claim_loop_for_run(&self, loop_id: &str, started_at: DateTime<Utc>) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loops
             SET status = ?1,
                 started_at = COALESCE(?2, started_at)
             WHERE id = ?3 AND status != ?1",
            params![
                LoopStatus::Running.as_str(),
                started_at.timestamp(),
                loop_id,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Persist (or, with `None`, clear) the pool a run against `loop_id` is
    /// currently drawing from. Called once when a run starts — including a
    /// resumed run, so a failed pool run that gets auto-reset-and-relaunched
    /// re-persists the same pool rather than losing it — and cleared again
    /// only when a run finishes genuinely. See
    /// [`crate::domain::loops::Loop::active_run_pool_id`].
    pub fn set_loop_active_run_pool(&self, loop_id: &str, pool_id: Option<&str>) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loops SET active_run_pool_id = ?1 WHERE id = ?2",
            params![pool_id, loop_id],
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

    /// Administratively transition a standalone spec's status. The transition
    /// is recorded with provenance (completed_via = 'admin') and the given reason.
    pub fn set_spec_admin_status(
        &self,
        spec_id: &str,
        status: LoopSpecStatus,
        reason: &str,
    ) -> Result<SpecAdminStatusOutcome> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        // 1. Check if spec exists
        let mut stmt = conn.prepare("SELECT id, loop_id FROM loop_specs WHERE id = ?1")?;
        let (_, loop_id): (String, Option<String>) = match stmt
            .query_row(params![spec_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
        {
            Some(row) => row,
            None => return Ok(SpecAdminStatusOutcome::NotFound),
        };

        // 2. Reject if spec is bound to a loop (not standalone)
        if let Some(loop_id) = loop_id {
            return Ok(SpecAdminStatusOutcome::NotStandalone(loop_id));
        }

        // 3. Check for active loop run
        if let Some(run) = active_loop_run_for_spec_locked(&conn, spec_id)? {
            return Ok(SpecAdminStatusOutcome::ActiveRun {
                loop_id: run.loop_id,
                run_id: run.id,
            });
        }

        // 4. Update the spec with admin status
        let now = Utc::now().timestamp();
        let (completed_at, completed_via_at) = match status {
            LoopSpecStatus::Pending => (None, None),
            _ => (Some(now), Some(now)),
        };

        conn.execute(
            "UPDATE loop_specs
             SET status = ?1, completed_at = ?2, completed_via = 'admin',
                 completed_via_reason = ?3, completed_via_at = ?4
             WHERE id = ?5",
            params![
                status.as_str(),
                completed_at,
                reason,
                completed_via_at,
                spec_id,
            ],
        )?;

        Ok(SpecAdminStatusOutcome::Success)
    }

    /// The single state-transition path behind `loop_reset` — resets a loop
    /// (and, without `specs`, every non-completed spec) back to `pending` so
    /// it can be relaunched. Shared by the `loop_reset` MCP tool and the
    /// scheduler's auto-reset-and-resume of a `failed` loop on autorun, so
    /// there is exactly one place that knows how to unstick a loop.
    ///
    /// When the loop's last run was against a pool (`active_run_pool_id` is
    /// set), the pool's *members* are what actually need resetting — the
    /// loop's own bound specs are typically empty for a pool run — so they're
    /// folded into the same eligible set as the loop's bound specs, both for
    /// validating an explicit `specs` list and for the "every non-completed"
    /// default. This is the one reset implementation both `loop_reset` and
    /// the scheduler's autorun share; it must not be forked.
    pub fn reset_loop(&self, loop_id: &str, specs: Option<&[String]>) -> Result<LoopResetOutcome> {
        let Some(lp) = self.get_loop(loop_id)? else {
            return Ok(LoopResetOutcome::NotFound);
        };

        if lp.status == LoopStatus::Running {
            return Ok(LoopResetOutcome::Running);
        }

        let bound_specs = self.list_loop_specs(loop_id)?;
        let pool_specs: Vec<LoopSpec> = match &lp.active_run_pool_id {
            Some(pool_id) => self
                .list_pool_member_spec_ids(pool_id)?
                .into_iter()
                .filter_map(|spec_id| self.get_loop_spec(&spec_id).transpose())
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        let eligible_specs: Vec<&LoopSpec> = bound_specs.iter().chain(pool_specs.iter()).collect();

        let valid_ids: std::collections::HashSet<&str> =
            eligible_specs.iter().map(|spec| spec.id.as_str()).collect();

        let target_ids: Vec<String> = match specs {
            Some(ids) => {
                for id in ids {
                    if !valid_ids.contains(id.as_str()) {
                        return Ok(LoopResetOutcome::InvalidSpec(id.clone()));
                    }
                }
                ids.to_vec()
            }
            None => eligible_specs
                .iter()
                .filter(|spec| spec.status != LoopSpecStatus::Completed)
                .map(|spec| spec.id.clone())
                .collect(),
        };

        for spec_id in &target_ids {
            // B12: a spec being reset can still have a `running` node-run row
            // left over from an interrupted attempt — the loop itself is
            // already non-`Running` here (the guard above refuses otherwise),
            // but that doesn't mean every spec's last run was cleanly
            // finalized (e.g. the loop failed on a *different* spec, or the
            // daemon crashed mid-node). Kill its process, if it still has
            // one, before wiping the spec back to `pending`, so a fresh run
            // never races a still-alive leftover in the same workdir.
            if let Some(stale) = self.get_active_loop_run_for_spec(spec_id)? {
                if let Some(pid) = stale.pid {
                    crate::daemon::process::terminate_process_group_async(
                        pid,
                        crate::daemon::process::KILL_GRACE,
                    );
                }
                self.update_loop_run_result(
                    &stale.id,
                    LoopRunStatus::Fail,
                    Some(&serde_json::json!({ "terminated": true, "reason": "spec reset" })),
                    Some(Utc::now()),
                )?;
            }
            self.reset_loop_spec_status(spec_id)?;
        }
        self.reset_loop_status(loop_id)?;

        Ok(LoopResetOutcome::Reset {
            spec_count: target_ids.len(),
        })
    }

    pub fn insert_loop_spec(&self, spec: &LoopSpec) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_specs (id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head, workdir)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                &spec.workdir,
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
            "SELECT id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head, workdir, completed_via, completed_via_reason, completed_via_at
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
            "SELECT id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head, workdir, completed_via, completed_via_reason, completed_via_at
             FROM loop_specs WHERE id = ?1",
        )?;

        stmt.query_row(params![spec_id], map_loop_spec_row)
            .optional()
            .map_err(Into::into)
    }

    /// A single spec's own graph (nodes/edges), resolved by spec id alone —
    /// independent of whether the spec is bound to a loop (`loop_specs.loop_id`)
    /// or a standalone pool member. Lets the loop engine drive a pool spec
    /// through the same lookup path as a bound spec (see `loop_engine::run`).
    pub fn get_loop_spec_details(&self, spec_id: &str) -> Result<Option<LoopSpecDetails>> {
        let Some(spec) = self.get_loop_spec(spec_id)? else {
            return Ok(None);
        };
        let nodes = self.list_loop_nodes(spec_id)?;
        let edges = self.list_loop_edges(spec_id)?;
        Ok(Some(LoopSpecDetails { spec, nodes, edges }))
    }

    /// Standalone specs, i.e. the backlog: specs not (yet) assigned to any
    /// loop, optionally filtered by their `workdir` tag and/or status.
    /// `unassigned_only` additionally filters to `loop_id IS NULL` — set it
    /// to `false` to see every spec regardless of loop assignment.
    pub fn list_specs(
        &self,
        workdir: Option<&str>,
        status: Option<LoopSpecStatus>,
        unassigned_only: bool,
    ) -> Result<Vec<LoopSpec>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head, workdir, completed_via, completed_via_reason, completed_via_at
             FROM loop_specs
             WHERE (?1 IS NULL OR workdir = ?1)
               AND (?2 IS NULL OR status = ?2)
               AND (?3 = 0 OR loop_id IS NULL)
             ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map(
            params![workdir, status.map(LoopSpecStatus::as_str), unassigned_only],
            map_loop_spec_row,
        )?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Update a standalone/backlog spec's name, description, and/or workdir
    /// tag. Unlike [`Self::update_loop_spec_details`] (position/parallelizable,
    /// used by `loop_update_spec`), this is for `spec_update` and never
    /// touches loop assignment or ordering.
    pub fn update_spec_tag_details(
        &self,
        spec_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        workdir: Option<Option<&str>>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_specs
             SET name = COALESCE(?1, name),
                 description = COALESCE(?2, description),
                 workdir = CASE WHEN ?3 IS NULL THEN workdir ELSE ?4 END
             WHERE id = ?5",
            params![
                name,
                description,
                workdir.map(|_| 1),
                workdir.flatten(),
                spec_id
            ],
        )?;
        Ok(rows > 0)
    }

    /// Delete a spec outright. Callers must enforce the loop-binding guard
    /// (see `spec_delete`'s handler) before calling this — this function
    /// performs no such check itself.
    pub fn delete_loop_spec(&self, spec_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute("DELETE FROM loop_specs WHERE id = ?1", params![spec_id])?;
        Ok(rows > 0)
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
        validate_single_target(node.spec_id.as_deref(), node.loop_id.as_deref())?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_nodes (id, spec_id, loop_id, name, kind, config, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &node.id,
                &node.spec_id,
                &node.loop_id,
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
            "SELECT id, spec_id, loop_id, name, kind, config, position, created_at
             FROM loop_nodes WHERE spec_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_loop_node_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Nodes belonging to a loop's top-level graph (as opposed to any one
    /// spec's graph).
    pub fn list_loop_nodes_for_loop(&self, loop_id: &str) -> Result<Vec<LoopNode>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, loop_id, name, kind, config, position, created_at
             FROM loop_nodes WHERE loop_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![loop_id], map_loop_node_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_loop_node(&self, node_id: &str) -> Result<Option<LoopNode>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, loop_id, name, kind, config, position, created_at
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
        validate_single_target(edge.spec_id.as_deref(), edge.loop_id.as_deref())?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_edges (id, spec_id, loop_id, from_node, to_node, condition)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &edge.id,
                &edge.spec_id,
                &edge.loop_id,
                &edge.from_node,
                &edge.to_node,
                edge.condition.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Insert a node together with any edges wiring it, in one transaction, so
    /// a copy (`loop_copy_node`) can never leave a node half-wired. Every edge
    /// and the node must target the same single graph (spec or loop).
    pub fn insert_node_with_edges(&self, node: &LoopNode, edges: &[LoopEdge]) -> Result<()> {
        validate_single_target(node.spec_id.as_deref(), node.loop_id.as_deref())?;
        for edge in edges {
            validate_single_target(edge.spec_id.as_deref(), edge.loop_id.as_deref())?;
        }
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO loop_nodes (id, spec_id, loop_id, name, kind, config, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &node.id,
                &node.spec_id,
                &node.loop_id,
                &node.name,
                node.kind.as_str(),
                serde_json::to_string(&node.config)?,
                node.position,
                node.created_at.timestamp(),
            ],
        )?;
        for edge in edges {
            tx.execute(
                "INSERT INTO loop_edges (id, spec_id, loop_id, from_node, to_node, condition)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    &edge.id,
                    &edge.spec_id,
                    &edge.loop_id,
                    &edge.from_node,
                    &edge.to_node,
                    edge.condition.as_str(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_loop_edges(&self, spec_id: &str) -> Result<Vec<LoopEdge>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, loop_id, from_node, to_node, condition
             FROM loop_edges WHERE spec_id = ?1 ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_loop_edge_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Edges belonging to a loop's top-level graph (as opposed to any one
    /// spec's graph).
    pub fn list_loop_edges_for_loop(&self, loop_id: &str) -> Result<Vec<LoopEdge>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, loop_id, from_node, to_node, condition
             FROM loop_edges WHERE loop_id = ?1 ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map(params![loop_id], map_loop_edge_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_loop_edge(&self, edge_id: &str) -> Result<Option<LoopEdge>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, loop_id, from_node, to_node, condition
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
            "INSERT INTO loop_runs (id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                run.pid,
                &run.boot_id,
                &run.session_id,
            ],
        )?;
        Ok(())
    }

    /// Record the OS process-group leader spawned for `run_id`'s node
    /// execution, and the boot it was spawned under. Called right after a
    /// successful `spawn()` — before that, the run row (inserted by the
    /// caller before execution starts) has `pid = NULL`, meaning "no live
    /// process to kill" (e.g. a gate node, or an agent/check node that
    /// hasn't finished spawning yet).
    pub fn set_loop_run_pid(&self, run_id: &str, pid: i64, boot_id: Option<&str>) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_runs SET pid = ?1, boot_id = ?2 WHERE id = ?3",
            params![pid, boot_id, run_id],
        )?;
        Ok(rows > 0)
    }

    /// Record the harness session id captured for a node run (RS1), so the
    /// session can be resumed later (RS2/RS3).
    pub fn set_loop_run_session_id(&self, run_id: &str, session_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_runs SET session_id = ?1 WHERE id = ?2",
            params![session_id, run_id],
        )?;
        Ok(rows > 0)
    }

    /// The active (`running`) node run for `spec_id`, if any. Mirrors
    /// [`Self::get_active_loop_run_for_node`] but scoped to a whole spec —
    /// used by `loop_reset`, which resets a spec wholesale rather than one
    /// node at a time.
    pub fn get_active_loop_run_for_spec(&self, spec_id: &str) -> Result<Option<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        active_loop_run_for_spec_locked(&conn, spec_id).map_err(Into::into)
    }

    /// Every node run still `running` across every loop — used at daemon
    /// shutdown to terminate every process this boot owns before exiting.
    pub fn list_all_running_loop_runs(&self) -> Result<Vec<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id
             FROM loop_runs WHERE status = 'running'",
        )?;
        let rows = stmt.query_map(params![], map_loop_run_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn list_loop_runs_for_spec(&self, spec_id: &str) -> Result<Vec<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id
             FROM loop_runs WHERE spec_id = ?1 ORDER BY started_at ASC, iteration ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_loop_run_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// All node runs recorded against `loop_id`, regardless of whether the
    /// spec they belong to is bound (`loop_specs.loop_id`) or was picked up
    /// live from a pool (pool members always keep `loop_id: None` on their
    /// own row — see `LoopEngine::run_loop`). `loop_runs.loop_id` is set on
    /// every insert either way, so this is the only reliable way to find a
    /// pool-driven loop's current/recent activity without a pool id in hand.
    pub fn list_loop_runs_for_loop(&self, loop_id: &str) -> Result<Vec<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id
             FROM loop_runs WHERE loop_id = ?1 ORDER BY started_at ASC, iteration ASC",
        )?;
        let rows = stmt.query_map(params![loop_id], map_loop_run_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_loop_run(&self, run_id: &str) -> Result<Option<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id
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
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id
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
                 completed_at = COALESCE(?3, completed_at),
                 pid = NULL
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

    /// Record one firing of a loop's `on_completed` hook (N2), started as
    /// `Running` before the process is spawned — mirrors [`Self::insert_loop_run`]'s
    /// pattern of a row that exists before the child does, so a crash mid-spawn
    /// still leaves a `Running` row behind rather than nothing.
    pub fn insert_loop_completion_hook_run(&self, run: &LoopCompletionHookRun) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_completion_hook_runs (id, loop_id, status, output, summary, started_at, completed_at, pid, boot_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &run.id,
                &run.loop_id,
                run.status.as_str(),
                run.output.as_ref().map(serde_json::to_string).transpose()?,
                &run.summary,
                run.started_at.timestamp(),
                run.completed_at.map(|value| value.timestamp()),
                run.pid,
                &run.boot_id,
            ],
        )?;
        Ok(())
    }

    /// Same B12 treatment as [`Self::set_loop_run_pid`]: record the spawned
    /// process-group leader so an abnormal end can `killpg` it.
    pub fn set_loop_completion_hook_run_pid(
        &self,
        run_id: &str,
        pid: i64,
        boot_id: Option<&str>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_completion_hook_runs SET pid = ?1, boot_id = ?2 WHERE id = ?3",
            params![pid, boot_id, run_id],
        )?;
        Ok(rows > 0)
    }

    pub fn update_loop_completion_hook_run_result(
        &self,
        run_id: &str,
        status: LoopRunStatus,
        output: Option<&Value>,
        summary: Option<&str>,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE loop_completion_hook_runs
             SET status = ?1,
                 output = COALESCE(?2, output),
                 summary = COALESCE(?3, summary),
                 completed_at = COALESCE(?4, completed_at),
                 pid = NULL
             WHERE id = ?5",
            params![
                status.as_str(),
                output.map(serde_json::to_string).transpose()?,
                summary,
                completed_at.map(|value| value.timestamp()),
                run_id,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Every past `on_completed` firing for a loop, oldest first — surfaced
    /// via `loop_get`/`canopy loop info` alongside the graph's node runs.
    pub fn list_loop_completion_hook_runs(
        &self,
        loop_id: &str,
    ) -> Result<Vec<LoopCompletionHookRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, status, output, summary, started_at, completed_at, pid, boot_id
             FROM loop_completion_hook_runs WHERE loop_id = ?1 ORDER BY started_at ASC",
        )?;
        let rows = stmt.query_map(params![loop_id], map_loop_completion_hook_run_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Node runs still `running` for a loop.
    pub fn list_running_loop_runs(&self, loop_id: &str) -> Result<Vec<LoopNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id
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
    /// daemon. Pause it, mark its dangling node runs as failed/interrupted,
    /// and reset its in-flight spec (loop-bound or pool member — either way
    /// `run.spec_id` names it) from `running` back to `pending`, all in one
    /// transaction so there is no window where the loop is recoverable but
    /// the spec is not (B18). A spec's completed work is preserved by the
    /// worktree/commits, not by its status, so restarting it from its entry
    /// node on resume is safe — and required: leaving it `running` made it
    /// invisible to pool selection (`pool_next_pending_spec_id` only ever
    /// picks a `pending` member), permanently orphaning it. `loop_continue`
    /// alone is enough to resume it (no `loop_pause` detour needed).
    /// Idempotent: a loop already `Paused` isn't touched by a later call.
    pub fn reconcile_orphaned_loops(&self) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let tx = conn.unchecked_transaction()?;

        let orphaned: Vec<Loop> = {
            let mut stmt = tx.prepare(
                "SELECT id, name, description, workdir, status, trigger_config, created_at, started_at, completed_at, autorun_at, active_run_pool_id, on_completed
                 FROM loops WHERE status = ?1",
            )?;
            let rows = stmt.query_map(params![LoopStatus::Running.as_str()], map_loop_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for lp in &orphaned {
            let dangling_runs: Vec<LoopNodeRun> = {
                let mut stmt = tx.prepare(
                    "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id
                     FROM loop_runs WHERE loop_id = ?1 AND status = 'running'",
                )?;
                let rows = stmt.query_map(params![lp.id], map_loop_run_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            if dangling_runs.is_empty() {
                tracing::warn!(
                    "Reconciling orphaned loop '{}': no active node run found; pausing.",
                    lp.id
                );
            }
            for run in &dangling_runs {
                tracing::warn!(
                    "Reconciling orphaned loop '{}': was running node '{}' (spec '{}') when the daemon last stopped; pausing loop, marking its run as interrupted, and resetting the spec to pending.",
                    lp.id,
                    run.node_id,
                    run.spec_id
                );
                // B12: this new daemon process never held a `Child` for
                // `run` — it may not even share the previous process's
                // memory — so a persisted pid is all reconciliation has to
                // go on, and a pid alone can't tell a genuine survivor from
                // an unrelated process that reused the same pid after a
                // reboot recycled the pid space. Only attempt the kill when
                // the run's recorded boot id still matches the machine's
                // current one (same boot, i.e. the *daemon* crashed/restarted
                // without the OS rebooting) — otherwise the pid is
                // meaningless and killing it could hit an unrelated process.
                if let (Some(pid), Some(run_boot_id)) = (run.pid, run.boot_id.as_deref()) {
                    if crate::system::boot_id().as_deref() == Some(run_boot_id) {
                        tracing::warn!(
                            "Reconciling orphaned loop '{}': attempting best-effort kill of survivor pid {} from the same boot.",
                            lp.id,
                            pid
                        );
                        crate::daemon::process::terminate_process_group_async(
                            pid,
                            crate::daemon::process::KILL_GRACE,
                        );
                    }
                }
                let now = Utc::now();
                tx.execute(
                    "UPDATE loop_runs
                     SET status = ?1, output = ?2, completed_at = ?3, pid = NULL
                     WHERE id = ?4",
                    params![
                        LoopRunStatus::Fail.as_str(),
                        serde_json::to_string(&serde_json::json!({
                            "interrupted": true,
                            "reason": "daemon restarted while this node was running"
                        }))?,
                        now.timestamp(),
                        run.id,
                    ],
                )?;
                // B18: same reset `reset_loop_spec_status` performs (status,
                // started_at, completed_at, spec_start_head all cleared) —
                // done inline here, against `tx`, rather than by calling that
                // method, since it would try to re-lock `self.conn` and
                // deadlock against the lock already held above.
                tx.execute(
                    "UPDATE loop_specs
                     SET status = ?1, started_at = NULL, completed_at = NULL, spec_start_head = NULL
                     WHERE id = ?2",
                    params![LoopSpecStatus::Pending.as_str(), run.spec_id],
                )?;
            }
            tx.execute(
                "UPDATE loops SET status = ?1 WHERE id = ?2",
                params![LoopStatus::Paused.as_str(), lp.id],
            )?;
        }

        tx.commit()?;
        Ok(orphaned.len())
    }

    /// Reset pool-member specs stuck `running` with no active node run in
    /// this daemon's lifetime back to `pending`. Covers the gap between
    /// `reconcile_orphaned_loops` (which only touches loops that were
    /// themselves `Running` at boot) and a pool member left `running` by a
    /// path that paused the loop without resetting the spec (e.g. a
    /// BLOCKER-reported spec that was never cleaned up). Called at server
    /// startup after `reconcile_orphaned_loops`.
    pub fn reconcile_stranded_pool_specs(&self) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let tx = conn.unchecked_transaction()?;

        let paused_loops: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, active_run_pool_id FROM loops
                 WHERE status = ?1 AND active_run_pool_id IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![LoopStatus::Paused.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut reset_count = 0;
        let boot_id = crate::system::boot_id();
        let current_boot_id = boot_id.as_deref();

        for (loop_id, pool_id) in &paused_loops {
            let stranded_specs: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT pm.spec_id FROM pool_members pm
                     JOIN loop_specs ls ON ls.id = pm.spec_id
                     WHERE pm.pool_id = ?1 AND ls.status = ?2
                     AND NOT EXISTS (
                         SELECT 1 FROM loop_runs lr
                         WHERE lr.spec_id = pm.spec_id
                         AND lr.status = 'running'
                         AND lr.boot_id = ?3
                     )
                     ORDER BY pm.position ASC",
                )?;
                let rows = stmt.query_map(
                    params![pool_id, LoopSpecStatus::Running.as_str(), current_boot_id],
                    |row| row.get::<_, String>(0),
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };

            for spec_id in &stranded_specs {
                tracing::warn!(
                    "Reconciling stranded pool spec '{}' in loop '{}': \
                     was 'running' with no active node run in this daemon's \
                     lifetime; resetting to pending.",
                    spec_id,
                    loop_id
                );
                tx.execute(
                    "UPDATE loop_specs
                     SET status = ?1, started_at = NULL, completed_at = NULL,
                         spec_start_head = NULL
                     WHERE id = ?2",
                    params![LoopSpecStatus::Pending.as_str(), spec_id],
                )?;
                reset_count += 1;
            }
        }

        tx.commit()?;
        Ok(reset_count)
    }

    pub fn get_loop_details(&self, loop_id: &str) -> Result<Option<LoopDetails>> {
        let Some(lp) = self.get_loop(loop_id)? else {
            return Ok(None);
        };
        let graph_nodes = self.list_loop_nodes_for_loop(loop_id)?;
        let graph_edges = self.list_loop_edges_for_loop(loop_id)?;
        let specs = self
            .list_loop_specs(loop_id)?
            .into_iter()
            .map(|spec| {
                let nodes = self.list_loop_nodes(&spec.id)?;
                let edges = self.list_loop_edges(&spec.id)?;
                Ok(LoopSpecDetails { spec, nodes, edges })
            })
            .collect::<Result<Vec<_>>>()?;
        let completion_hook_runs = self.list_loop_completion_hook_runs(loop_id)?;

        Ok(Some(LoopDetails {
            lp,
            graph_nodes,
            graph_edges,
            specs,
            completion_hook_runs,
        }))
    }
}

/// A loop node/edge must target exactly one of `spec_id`/`loop_id` — never
/// both (ambiguous ownership) and never neither (orphaned row no graph
/// would ever load). Checked here, before the row ever reaches the DB, so
/// callers get an actionable message instead of a raw `CHECK constraint
/// failed` from SQLite.
fn validate_single_target(spec_id: Option<&str>, loop_id: Option<&str>) -> Result<()> {
    match (spec_id, loop_id) {
        (Some(_), Some(_)) => Err(anyhow!(
            "Loop node/edge must target exactly one of spec_id or loop_id, not both."
        )),
        (None, None) => Err(anyhow!(
            "Loop node/edge must target exactly one of spec_id or loop_id."
        )),
        _ => Ok(()),
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
        active_run_pool_id: row.get(10)?,
        on_completed: row
            .get::<_, Option<String>>(11)?
            .as_deref()
            .map(decode_loop_completion_hook)
            .transpose()?,
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

/// Encode a loop's `on_completed` hook config as JSON for the `on_completed`
/// column. `None` (no hook configured) stores `NULL` — exactly today's
/// (pre-N2) row shape.
fn encode_loop_completion_hook(hook: Option<&LoopCompletionHook>) -> Result<Option<String>> {
    hook.map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

/// Decode the `on_completed` column JSON back into a [`LoopCompletionHook`].
fn decode_loop_completion_hook(raw: &str) -> rusqlite::Result<LoopCompletionHook> {
    serde_json::from_str(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(error))
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
        workdir: row.get(10)?,
        completed_via: row.get(11)?,
        completed_via_reason: row.get(12)?,
        completed_via_at: row
            .get::<_, Option<i64>>(13)?
            .map(from_timestamp)
            .transpose()?,
    })
}

fn map_loop_node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoopNode> {
    let kind = LoopNodeKind::from_str(&row.get::<_, String>(4)?).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(IoError::new(
                ErrorKind::InvalidData,
                "Invalid loop node kind",
            )),
        )
    })?;
    let config_raw: String = row.get(5)?;
    let config = parse_json_value(&config_raw)?;

    Ok(LoopNode {
        id: row.get(0)?,
        spec_id: row.get(1)?,
        loop_id: row.get(2)?,
        name: row.get(3)?,
        kind,
        config,
        position: row.get(6)?,
        created_at: from_timestamp(row.get(7)?)?,
    })
}

fn map_loop_edge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoopEdge> {
    let condition = LoopEdgeCondition::from_str(&row.get::<_, String>(5)?).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
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
        loop_id: row.get(2)?,
        from_node: row.get(3)?,
        to_node: row.get(4)?,
        condition,
    })
}

fn active_loop_run_for_spec_locked(
    conn: &rusqlite::Connection,
    spec_id: &str,
) -> rusqlite::Result<Option<LoopNodeRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, loop_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration, pid, boot_id, session_id
         FROM loop_runs
         WHERE spec_id = ?1 AND status = 'running'
         ORDER BY started_at DESC
         LIMIT 1",
    )?;
    stmt.query_row(params![spec_id], map_loop_run_row)
        .optional()
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
        pid: row.get(10)?,
        boot_id: row.get(11)?,
        session_id: row.get(12)?,
    })
}

fn map_loop_completion_hook_run_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<LoopCompletionHookRun> {
    Ok(LoopCompletionHookRun {
        id: row.get(0)?,
        loop_id: row.get(1)?,
        status: LoopRunStatus::from_str(&row.get::<_, String>(2)?),
        output: row
            .get::<_, Option<String>>(3)?
            .as_deref()
            .map(parse_json_value)
            .transpose()?,
        summary: row.get(4)?,
        started_at: from_timestamp(row.get(5)?)?,
        completed_at: row
            .get::<_, Option<i64>>(6)?
            .map(from_timestamp)
            .transpose()?,
        pid: row.get(7)?,
        boot_id: row.get(8)?,
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
