use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use std::io::{Error as IoError, ErrorKind};

use crate::db::Database;
use crate::domain::loops::LoopSpecStatus;
use crate::domain::pools::{Pool, PoolDetails};

impl Database {
    pub fn insert_pool(&self, pool: &Pool) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO pools (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![&pool.id, &pool.name, pool.created_at.timestamp()],
        )?;
        Ok(())
    }

    pub fn get_pool(&self, pool_id: &str) -> Result<Option<Pool>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare("SELECT id, name, created_at FROM pools WHERE id = ?1")?;
        stmt.query_row(params![pool_id], map_pool_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn list_pools(&self) -> Result<Vec<Pool>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt =
            conn.prepare("SELECT id, name, created_at FROM pools ORDER BY created_at ASC")?;
        let rows = stmt.query_map([], map_pool_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Spec ids belonging to `pool_id`, in queue order.
    pub fn list_pool_member_spec_ids(&self, pool_id: &str) -> Result<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn
            .prepare("SELECT spec_id FROM pool_members WHERE pool_id = ?1 ORDER BY position ASC")?;
        let rows = stmt.query_map(params![pool_id], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn pool_has_member(&self, pool_id: &str, spec_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pool_members WHERE pool_id = ?1 AND spec_id = ?2",
            params![pool_id, spec_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Append `spec_id` to the end of `pool_id`'s queue: one past the
    /// highest existing position, or 1 for an empty pool. `group_name` (RS3)
    /// is the optional context group the member joins — `None` for an
    /// ungrouped member, which never cross-resumes another spec's session.
    pub fn append_pool_member(
        &self,
        pool_id: &str,
        spec_id: &str,
        group_name: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let next_position: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM pool_members WHERE pool_id = ?1",
            params![pool_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO pool_members (pool_id, spec_id, position, group_name) VALUES (?1, ?2, ?3, ?4)",
            params![pool_id, spec_id, next_position, group_name],
        )?;
        Ok(())
    }

    /// The RS3 context group `spec_id` belongs to within `pool_id`, or `None`
    /// if the spec is ungrouped or not a member.
    pub fn pool_member_group(&self, pool_id: &str, spec_id: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.query_row(
            "SELECT group_name FROM pool_members WHERE pool_id = ?1 AND spec_id = ?2",
            params![pool_id, spec_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// `(spec_id, group_name)` for every member of `pool_id`, in queue order —
    /// the group-aware companion to [`Self::list_pool_member_spec_ids`].
    pub fn list_pool_member_groups(&self, pool_id: &str) -> Result<Vec<(String, Option<String>)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT spec_id, group_name FROM pool_members WHERE pool_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![pool_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// RS3 group-session handoff: the harness session id a grouped spec should
    /// RESUME on its first visit to `node_id`, derived entirely from the DB so
    /// a daemon restart mid-queue never loses group context.
    ///
    /// The seed is the session captured on `node_id` by the group's immediately
    /// preceding sibling — the grouped member with the greatest position below
    /// `spec_id` that has reached a terminal state (`completed`/`failed`).
    /// Taint is enforced by only ever consulting that single nearest terminal
    /// sibling: if it `failed` (or exhausted its budget, which also marks it
    /// `failed`) the chain is broken and this returns `None` (cold start), and
    /// an earlier `completed` sibling behind the failure is never resurrected.
    /// A `completed` sibling that captured no session on `node_id` likewise
    /// yields `None`. `skipped` (and other non-terminal) siblings are stepped
    /// over — they neither taint the chain nor supply a session.
    pub fn group_session_for_node(
        &self,
        pool_id: &str,
        group_name: &str,
        spec_id: &str,
        node_id: &str,
    ) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        // The nearest terminal grouped sibling ahead of `spec_id` in the queue.
        let predecessor: Option<(String, String)> = conn
            .query_row(
                "SELECT pm.spec_id, ls.status
                 FROM pool_members pm
                 JOIN loop_specs ls ON ls.id = pm.spec_id
                 WHERE pm.pool_id = ?1 AND pm.group_name = ?2
                   AND pm.position < (
                       SELECT position FROM pool_members
                       WHERE pool_id = ?1 AND spec_id = ?3
                   )
                   AND ls.status IN (?4, ?5)
                 ORDER BY pm.position DESC
                 LIMIT 1",
                params![
                    pool_id,
                    group_name,
                    spec_id,
                    LoopSpecStatus::Completed.as_str(),
                    LoopSpecStatus::Failed.as_str(),
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;

        // Taint: a failed (or budget-exhausted) nearest sibling breaks the
        // chain — cold start, and never fall back to an earlier completed one.
        let Some((predecessor_spec, status)) = predecessor else {
            return Ok(None);
        };
        if status != LoopSpecStatus::Completed.as_str() {
            return Ok(None);
        }

        // The session that most recently served `node_id` for that completed
        // sibling — the warm context to continue.
        conn.query_row(
            "SELECT session_id FROM loop_runs
             WHERE spec_id = ?1 AND node_id = ?2 AND session_id IS NOT NULL
             ORDER BY started_at DESC, rowid DESC
             LIMIT 1",
            params![predecessor_spec, node_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Display helper (RS3): the group label whose warm session this run
    /// continued, or `None` if the run was a cold start. A run counts as a
    /// group-resume when its `session_id` was also captured on the same
    /// `node_id` by a *different* grouped sibling in the same queue.
    pub fn group_resume_source(
        &self,
        pool_id: &str,
        spec_id: &str,
        node_id: &str,
        session_id: &str,
    ) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.query_row(
            "SELECT pm.group_name
             FROM pool_members pm
             WHERE pm.pool_id = ?1 AND pm.spec_id = ?2 AND pm.group_name IS NOT NULL
               AND EXISTS (
                   SELECT 1 FROM pool_members sib
                   JOIN loop_runs lr ON lr.spec_id = sib.spec_id
                   WHERE sib.pool_id = ?1 AND sib.group_name = pm.group_name
                     AND sib.spec_id != ?2
                     AND lr.node_id = ?3 AND lr.session_id = ?4
               )",
            params![pool_id, spec_id, node_id, session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    pub fn remove_pool_member(&self, pool_id: &str, spec_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "DELETE FROM pool_members WHERE pool_id = ?1 AND spec_id = ?2",
            params![pool_id, spec_id],
        )?;
        Ok(rows > 0)
    }

    /// Replace `pool_id`'s membership with `order`, positioned 1..=N in the
    /// given sequence. Callers must first validate that `order` is a total
    /// permutation of the pool's current members — this rebuilds the rows
    /// unconditionally, so an unvalidated `order` would silently drop or
    /// duplicate membership.
    pub fn reorder_pool_members(&self, pool_id: &str, order: &[String]) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        // RS3: preserve each member's context group across the rebuild — the
        // reorder only moves rows, it must never silently drop group membership.
        let mut groups: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT spec_id, group_name FROM pool_members WHERE pool_id = ?1")?;
            let rows = stmt.query_map(params![pool_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (spec_id, group_name) = row?;
                groups.insert(spec_id, group_name);
            }
        }

        conn.execute(
            "DELETE FROM pool_members WHERE pool_id = ?1",
            params![pool_id],
        )?;
        for (index, spec_id) in order.iter().enumerate() {
            let group_name = groups.get(spec_id).cloned().flatten();
            conn.execute(
                "INSERT INTO pool_members (pool_id, spec_id, position, group_name) VALUES (?1, ?2, ?3, ?4)",
                params![pool_id, spec_id, (index as i64) + 1, group_name],
            )?;
        }
        Ok(())
    }

    /// The pool's first PENDING member, in queue order — queried fresh on
    /// every call rather than off a list frozen at run start. This is what
    /// lets a live pool run pick up `pool_add_spec`/`pool_reorder` calls
    /// made while the run is in flight: the engine calls this again at every
    /// spec boundary instead of iterating a `Vec` captured once.
    pub fn pool_next_pending_spec_id(&self, pool_id: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.query_row(
            "SELECT pm.spec_id FROM pool_members pm
             JOIN loop_specs ls ON ls.id = pm.spec_id
             WHERE pm.pool_id = ?1 AND ls.status = ?2
             ORDER BY pm.position ASC LIMIT 1",
            params![pool_id, LoopSpecStatus::Pending.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Whether `pool_id` still has a member that isn't `completed`/`skipped`
    /// (i.e. `pending` or stuck `running`). Used by the loop engine as a
    /// guard against marking a pool run's loop `completed` when
    /// [`Self::pool_next_pending_spec_id`] finds no `pending` member to pick
    /// next but a member is nonetheless left non-terminal — e.g. `running`
    /// because a previous run crashed mid-spec and hasn't been reset yet.
    pub fn pool_has_incomplete_members(&self, pool_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pool_members pm
             JOIN loop_specs ls ON ls.id = pm.spec_id
             WHERE pm.pool_id = ?1 AND ls.status NOT IN (?2, ?3)",
            params![
                pool_id,
                LoopSpecStatus::Completed.as_str(),
                LoopSpecStatus::Skipped.as_str()
            ],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Pool members left `running` with no live node run behind them in this
    /// daemon's lifetime — a safety net for status left stuck `running` by a
    /// path other than G2 boot reconcile (which only ever reconciles a loop
    /// that was itself `Running` at boot; a member corrupted to `running` by
    /// some other route, or belonging to a loop reconcile didn't touch,
    /// would otherwise stay silently invisible to
    /// [`Self::pool_next_pending_spec_id`] forever). "Live in this daemon's
    /// lifetime" means a `loop_runs` row for the spec that is still
    /// `running` *and* stamped with the current process's boot id — matching
    /// [`Database::reconcile_orphaned_loops`]'s own liveness test. Returned
    /// in queue order; callers must log why before recovering one (R3: no
    /// spec status may silently exclude a member from selection).
    pub fn pool_stale_running_members(
        &self,
        pool_id: &str,
        current_boot_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT pm.spec_id FROM pool_members pm
             JOIN loop_specs ls ON ls.id = pm.spec_id
             WHERE pm.pool_id = ?1 AND ls.status = ?2
             AND NOT EXISTS (
                 SELECT 1 FROM loop_runs lr
                 WHERE lr.spec_id = pm.spec_id AND lr.status = 'running' AND lr.boot_id = ?3
             )
             ORDER BY pm.position ASC",
        )?;
        let rows = stmt.query_map(
            params![pool_id, LoopSpecStatus::Running.as_str(), current_boot_id],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_pool_details(&self, pool_id: &str) -> Result<Option<PoolDetails>> {
        let Some(pool) = self.get_pool(pool_id)? else {
            return Ok(None);
        };
        let member_pairs = self.list_pool_member_groups(pool_id)?;
        let member_groups = member_pairs
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        let members = member_pairs
            .into_iter()
            .filter_map(|(spec_id, _)| self.get_loop_spec(&spec_id).transpose())
            .collect::<Result<Vec<_>>>()?;

        Ok(Some(PoolDetails {
            pool,
            members,
            member_groups,
        }))
    }
}

fn map_pool_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Pool> {
    Ok(Pool {
        id: row.get(0)?,
        name: row.get(1)?,
        created_at: from_timestamp(row.get(2)?)?,
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
