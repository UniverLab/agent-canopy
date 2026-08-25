//! F1: DB layer for ensembles — a group of agent-node members (each
//! optionally carrying its own prompt override) plus their quorum gate,
//! persisted as one [`Ensemble`] row on top of ordinary
//! `loop_nodes`/`loop_edges` rows (see `src/domain/loops.rs` for why).

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use std::io::{Error as IoError, ErrorKind};

use crate::db::Database;
use crate::domain::blueprints::{builtin_ensemble_blueprint_specs, EnsembleBlueprint};
use crate::domain::loops::{
    Ensemble, EnsembleDetails, EnsembleMember, EnsembleMemberSpec, LoopEdge, LoopEdgeCondition,
    LoopNode,
};

impl Database {
    /// Create an entire ensemble unit — the join node, every member node,
    /// every wiring edge (entry fan-out, member→join fan-in, join exit
    /// routing), the `ensembles` row, and every `ensemble_members` row — in
    /// one transaction. This is the DB-layer half of the "one MCP call"
    /// contract: `loop_add_ensemble` builds all these pieces and hands them
    /// here so a crash mid-creation can never leave a half-expanded ensemble.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_ensemble_unit(
        &self,
        ensemble: &Ensemble,
        members: &[EnsembleMember],
        member_nodes: &[LoopNode],
        join_node: &LoopNode,
        edges: &[LoopEdge],
    ) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO loop_nodes (id, spec_id, loop_id, name, kind, config, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &join_node.id,
                &join_node.spec_id,
                &join_node.loop_id,
                &join_node.name,
                join_node.kind.as_str(),
                serde_json::to_string(&join_node.config)?,
                join_node.position,
                join_node.created_at.timestamp(),
            ],
        )?;

        for node in member_nodes {
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
        }

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

        tx.execute(
            "INSERT INTO ensembles (id, spec_id, loop_id, name, prompt_template, join_node_id, entry_from_node, entry_condition, min_pass, straggler_timeout_minutes, timeout_minutes, on_pass_to, on_fail_to, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                &ensemble.id,
                &ensemble.spec_id,
                &ensemble.loop_id,
                &ensemble.name,
                &ensemble.prompt_template,
                &ensemble.join_node_id,
                &ensemble.entry_from_node,
                ensemble.entry_condition.as_str(),
                ensemble.min_pass,
                ensemble.straggler_timeout_minutes,
                ensemble.timeout_minutes,
                &ensemble.on_pass_to,
                &ensemble.on_fail_to,
                ensemble.created_at.timestamp(),
            ],
        )?;

        for member in members {
            tx.execute(
                "INSERT INTO ensemble_members (ensemble_id, node_id, position, platform, model, prompt_override)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    &member.ensemble_id,
                    &member.node_id,
                    member.position,
                    &member.platform,
                    &member.model,
                    &member.prompt_override,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn get_ensemble(&self, ensemble_id: &str) -> Result<Option<Ensemble>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, loop_id, name, prompt_template, join_node_id, entry_from_node, entry_condition, min_pass, straggler_timeout_minutes, timeout_minutes, on_pass_to, on_fail_to, created_at
             FROM ensembles WHERE id = ?1",
        )?;
        stmt.query_row(params![ensemble_id], map_ensemble_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn list_ensemble_members(&self, ensemble_id: &str) -> Result<Vec<EnsembleMember>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT ensemble_id, node_id, position, platform, model, prompt_override
             FROM ensemble_members WHERE ensemble_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![ensemble_id], map_ensemble_member_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_ensemble_details(&self, ensemble_id: &str) -> Result<Option<EnsembleDetails>> {
        let Some(ensemble) = self.get_ensemble(ensemble_id)? else {
            return Ok(None);
        };
        let members = self.list_ensemble_members(ensemble_id)?;
        Ok(Some(EnsembleDetails { ensemble, members }))
    }

    /// Every ensemble defined directly on a spec's own graph.
    pub fn list_ensembles_for_spec(&self, spec_id: &str) -> Result<Vec<EnsembleDetails>> {
        let ids = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            let mut stmt = conn.prepare("SELECT id FROM ensembles WHERE spec_id = ?1")?;
            let rows = stmt.query_map(params![spec_id], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        ids.iter()
            .filter_map(|id| self.get_ensemble_details(id).transpose())
            .collect()
    }

    /// Every ensemble defined on a loop's top-level graph.
    pub fn list_ensembles_for_loop(&self, loop_id: &str) -> Result<Vec<EnsembleDetails>> {
        let ids = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            let mut stmt = conn.prepare("SELECT id FROM ensembles WHERE loop_id = ?1")?;
            let rows = stmt.query_map(params![loop_id], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        ids.iter()
            .filter_map(|id| self.get_ensemble_details(id).transpose())
            .collect()
    }

    /// The ensemble `node_id` is a member of, if any — the engine's fan-out
    /// detection and the write-time "no individual member overrides" guard
    /// both go through here.
    pub fn get_ensemble_by_member_node(&self, node_id: &str) -> Result<Option<EnsembleDetails>> {
        let ensemble_id: Option<String> = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            conn.query_row(
                "SELECT ensemble_id FROM ensemble_members WHERE node_id = ?1",
                params![node_id],
                |row| row.get(0),
            )
            .optional()?
        };
        match ensemble_id {
            Some(id) => self.get_ensemble_details(&id),
            None => Ok(None),
        }
    }

    /// The ensemble `node_id` is the join node of, if any.
    pub fn get_ensemble_by_join_node(&self, node_id: &str) -> Result<Option<EnsembleDetails>> {
        let ensemble_id: Option<String> = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            conn.query_row(
                "SELECT id FROM ensembles WHERE join_node_id = ?1",
                params![node_id],
                |row| row.get(0),
            )
            .optional()?
        };
        match ensemble_id {
            Some(id) => self.get_ensemble_details(&id),
            None => Ok(None),
        }
    }

    /// Update the shared prompt on the `ensembles` row itself. Propagating it
    /// onto every member node's `config` is the caller's job (via
    /// `update_loop_node_details`) — this only updates the source-of-truth
    /// row `loop_get` reads back.
    pub fn update_ensemble_prompt(&self, ensemble_id: &str, prompt_template: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE ensembles SET prompt_template = ?1 WHERE id = ?2",
            params![prompt_template, ensemble_id],
        )?;
        Ok(rows > 0)
    }

    /// Update the join's own config: pass threshold, straggler timeout, and
    /// shared member timeout. `None` means "leave unchanged" for each field —
    /// same convention as [`Self::update_loop_node_details`].
    pub fn update_ensemble_join_config(
        &self,
        ensemble_id: &str,
        min_pass: Option<i64>,
        straggler_timeout_minutes: Option<Option<i64>>,
        timeout_minutes: Option<i64>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE ensembles
             SET min_pass = COALESCE(?1, min_pass),
                 straggler_timeout_minutes = CASE WHEN ?2 IS NULL THEN straggler_timeout_minutes ELSE ?3 END,
                 timeout_minutes = COALESCE(?4, timeout_minutes)
             WHERE id = ?5",
            params![
                min_pass,
                straggler_timeout_minutes.map(|_| 1),
                straggler_timeout_minutes.flatten(),
                timeout_minutes,
                ensemble_id,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Update the join's exit routing (`on_pass_to`/`on_fail_to`) on the
    /// `ensembles` row. Recreating the underlying `loop_edges` rows is the
    /// caller's job (`loop_update_ensemble`) — this only updates the
    /// source-of-truth columns.
    pub fn update_ensemble_exit_wiring(
        &self,
        ensemble_id: &str,
        on_pass_to: Option<&str>,
        on_fail_to: Option<Option<&str>>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE ensembles
             SET on_pass_to = COALESCE(?1, on_pass_to),
                 on_fail_to = CASE WHEN ?2 IS NULL THEN on_fail_to ELSE ?3 END
             WHERE id = ?4",
            params![
                on_pass_to,
                on_fail_to.map(|_| 1),
                on_fail_to.flatten(),
                ensemble_id,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Add one new member — its node, `ensemble_members` row, entry edge
    /// (from the ensemble's own `entry_from_node`/`entry_condition`), and
    /// join edge — in one transaction. Used by `loop_update_ensemble` when
    /// growing the member list.
    pub fn add_ensemble_member(
        &self,
        member: &EnsembleMember,
        node: &LoopNode,
        entry_edge: &LoopEdge,
        join_edge: &LoopEdge,
    ) -> Result<()> {
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

        for edge in [entry_edge, join_edge] {
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

        tx.execute(
            "INSERT INTO ensemble_members (ensemble_id, node_id, position, platform, model, prompt_override)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &member.ensemble_id,
                &member.node_id,
                member.position,
                &member.platform,
                &member.model,
                &member.prompt_override,
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Remove a member outright: deletes its `loop_nodes` row, which cascades
    /// away its `ensemble_members` row and both wiring edges (entry, join) —
    /// every one of those FKs is `ON DELETE CASCADE`. Used by
    /// `loop_update_ensemble` when shrinking the member list.
    pub fn remove_ensemble_member(&self, node_id: &str) -> Result<bool> {
        self.delete_loop_node(node_id)
    }

    /// Update an existing member's `platform`/`model`/`prompt_override` in
    /// place — used by `loop_update_ensemble` when the member count is
    /// unchanged (only the fields at a given position changed). Always sets
    /// `prompt_override` outright (never "leave unchanged") since a
    /// replacement member list is always given in full. The caller separately
    /// updates the member node's own `config` via
    /// [`Self::update_loop_node_details`].
    pub fn update_ensemble_member(
        &self,
        ensemble_id: &str,
        node_id: &str,
        platform: &str,
        model: Option<&str>,
        prompt_override: Option<&str>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE ensemble_members SET platform = ?1, model = ?2, prompt_override = ?3 WHERE ensemble_id = ?4 AND node_id = ?5",
            params![platform, model, prompt_override, ensemble_id, node_id],
        )?;
        Ok(rows > 0)
    }

    /// Delete a loop node outright — cascades away its edges (both
    /// directions) and, if it was an ensemble member, its `ensemble_members`
    /// row too. General-purpose (not ensemble-specific); callers own any
    /// "is this safe to delete" guard.
    pub fn delete_loop_node(&self, node_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute("DELETE FROM loop_nodes WHERE id = ?1", params![node_id])?;
        Ok(rows > 0)
    }

    /// Delete every edge from `node_id` with the given `condition` — used by
    /// `loop_update_ensemble` to retarget the join's exit routing (delete the
    /// old pass/fail edge, then insert the new one).
    pub fn delete_loop_edges_from_node_with_condition(
        &self,
        node_id: &str,
        condition: &LoopEdgeCondition,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "DELETE FROM loop_edges WHERE from_node = ?1 AND condition = ?2",
            params![node_id, condition.as_str()],
        )?;
        Ok(())
    }

    pub fn insert_ensemble_blueprint(&self, blueprint: &EnsembleBlueprint) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO ensemble_blueprints (id, name, prompt_template, members, min_pass, builtin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &blueprint.id,
                &blueprint.name,
                &blueprint.prompt_template,
                serde_json::to_string(&blueprint.members)?,
                blueprint.min_pass,
                blueprint.builtin,
                blueprint.created_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn get_ensemble_blueprint_by_name(&self, name: &str) -> Result<Option<EnsembleBlueprint>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, prompt_template, members, min_pass, builtin, created_at
             FROM ensemble_blueprints WHERE name = ?1",
        )?;
        stmt.query_row(params![name], map_ensemble_blueprint_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn list_ensemble_blueprints(&self) -> Result<Vec<EnsembleBlueprint>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, prompt_template, members, min_pass, builtin, created_at
             FROM ensemble_blueprints ORDER BY builtin DESC, name ASC",
        )?;
        let rows = stmt.query_map([], map_ensemble_blueprint_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Delete an ensemble blueprint by name. Mirrors
    /// [`Self::delete_blueprint_by_name`]: callers own any builtin guard —
    /// this performs none itself.
    pub fn delete_ensemble_blueprint_by_name(&self, name: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "DELETE FROM ensemble_blueprints WHERE name = ?1",
            params![name],
        )?;
        Ok(rows > 0)
    }

    /// Overwrite a builtin ensemble blueprint row's `prompt_template`/
    /// `members`/`min_pass` in place — the ensemble mirror of
    /// [`Self::update_builtin_blueprint`], reconciling a stale builtin shape
    /// (e.g. a hardcoded model identity from before this field was dropped)
    /// with the current spec. Only ever called from
    /// [`Self::seed_builtin_ensemble_blueprints`].
    fn update_builtin_ensemble_blueprint(
        &self,
        id: &str,
        prompt_template: &str,
        members: &[EnsembleMemberSpec],
        min_pass: Option<i64>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE ensemble_blueprints SET prompt_template = ?1, members = ?2, min_pass = ?3 WHERE id = ?4",
            params![
                prompt_template,
                serde_json::to_string(members)?,
                min_pass,
                id,
            ],
        )?;
        Ok(())
    }

    /// Seed the builtin ensemble blueprints (currently just
    /// "ensemble-proposers") and keep already-seeded builtin rows in sync
    /// with the current `builtin_ensemble_blueprint_specs()` shape —
    /// idempotent and safe on every daemon startup, mirroring
    /// [`Self::seed_builtin_blueprints`]'s stale-name-delete /
    /// drifted-config-overwrite / leave-custom-rows-alone behavior exactly.
    pub fn seed_builtin_ensemble_blueprints(&self) -> Result<()> {
        let specs = builtin_ensemble_blueprint_specs();
        let current_names: std::collections::HashSet<&str> =
            specs.iter().map(|(name, ..)| *name).collect();

        for existing in self.list_ensemble_blueprints()? {
            if existing.builtin && !current_names.contains(existing.name.as_str()) {
                self.delete_ensemble_blueprint_by_name(&existing.name)?;
            }
        }

        for (name, prompt_template, members, min_pass) in specs {
            let members: Vec<EnsembleMemberSpec> = members
                .into_iter()
                .map(|(platform, model, prompt_override)| {
                    (
                        platform.to_string(),
                        model.map(str::to_string),
                        prompt_override.map(str::to_string),
                    )
                })
                .collect();

            match self.get_ensemble_blueprint_by_name(name)? {
                Some(existing) if existing.builtin => {
                    if existing.prompt_template != prompt_template
                        || existing.members != members
                        || existing.min_pass != min_pass
                    {
                        self.update_builtin_ensemble_blueprint(
                            &existing.id,
                            prompt_template,
                            &members,
                            min_pass,
                        )?;
                    }
                }
                // Name already claimed by a custom ensemble blueprint — leave it be.
                Some(_) => {}
                None => {
                    self.insert_ensemble_blueprint(&EnsembleBlueprint {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: name.to_string(),
                        prompt_template: prompt_template.to_string(),
                        members,
                        min_pass,
                        builtin: true,
                        created_at: Utc::now(),
                    })?;
                }
            }
        }
        Ok(())
    }
}

/// Parse a blueprint row's stored `members` JSON, tolerating both the
/// pre-prompt-override 2-element `[platform, model]` shape (rows seeded
/// before this field existed — including a builtin blueprint from an older
/// daemon) and the current 3-element `[platform, model, prompt_override]`
/// shape, so an upgrade never needs a data migration for blueprints already
/// in the DB.
fn parse_ensemble_blueprint_members(
    raw: &str,
) -> Result<Vec<EnsembleMemberSpec>, serde_json::Error> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(raw)?;
    Ok(rows
        .into_iter()
        .map(|entry| {
            let platform = entry
                .get(0)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let model = entry
                .get(1)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let prompt_override = entry
                .get(2)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            (platform, model, prompt_override)
        })
        .collect())
}

fn map_ensemble_blueprint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EnsembleBlueprint> {
    let members_raw: String = row.get(3)?;
    let members = parse_ensemble_blueprint_members(&members_raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(EnsembleBlueprint {
        id: row.get(0)?,
        name: row.get(1)?,
        prompt_template: row.get(2)?,
        members,
        min_pass: row.get(4)?,
        builtin: row.get(5)?,
        created_at: from_timestamp(row.get(6)?)?,
    })
}

fn map_ensemble_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Ensemble> {
    let entry_condition =
        LoopEdgeCondition::from_str(&row.get::<_, String>(7)?).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(IoError::new(
                    ErrorKind::InvalidData,
                    "Invalid ensemble entry_condition",
                )),
            )
        })?;
    Ok(Ensemble {
        id: row.get(0)?,
        spec_id: row.get(1)?,
        loop_id: row.get(2)?,
        name: row.get(3)?,
        prompt_template: row.get(4)?,
        join_node_id: row.get(5)?,
        entry_from_node: row.get(6)?,
        entry_condition,
        min_pass: row.get(8)?,
        straggler_timeout_minutes: row.get(9)?,
        timeout_minutes: row.get(10)?,
        on_pass_to: row.get(11)?,
        on_fail_to: row.get(12)?,
        created_at: from_timestamp(row.get(13)?)?,
    })
}

fn map_ensemble_member_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EnsembleMember> {
    Ok(EnsembleMember {
        ensemble_id: row.get(0)?,
        node_id: row.get(1)?,
        position: row.get(2)?,
        platform: row.get(3)?,
        model: row.get(4)?,
        prompt_override: row.get(5)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::loops::{LoopNodeKind, LoopSpec, LoopSpecStatus};
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Database {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Database::new(&path).expect("create test db")
    }

    fn insert_spec(db: &Database, id: &str) {
        db.insert_loop_spec(&LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: Some("Do the thing".to_string()),
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
    }

    /// The DB-layer half of the "one call -> N+1 nodes" contract: given the
    /// exact node/edge/ensemble shape `loop_add_ensemble` assembles for 3
    /// members, one `insert_ensemble_unit` transaction must leave behind 1
    /// join node + 3 member nodes (4 new `loop_nodes` rows total), the
    /// entry fan-out edge per member, the member->join fan-in edge per
    /// member, and the join's own pass edge — all in one shot.
    #[test]
    fn insert_ensemble_unit_creates_join_and_member_nodes_with_correct_wiring() {
        let db = test_db();
        insert_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "kickoff".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true", "success_condition": "exit_code_0"}),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "arbiter".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 10,
            created_at: Utc::now(),
        })
        .unwrap();

        let now = Utc::now();
        let member_ids = ["m1", "m2", "m3"];
        let member_nodes: Vec<LoopNode> = member_ids
            .iter()
            .enumerate()
            .map(|(i, id)| LoopNode {
                id: id.to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: format!("member-{}", i + 1),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({
                    "platform": "openrouter",
                    "prompt_template": "draft it",
                }),
                position: 2 + i as i64,
                created_at: now,
            })
            .collect();
        let join_node = LoopNode {
            id: "join1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({"ensemble_id": "ens1"}),
            position: 5,
            created_at: now,
        };
        let mut edges = Vec::new();
        for id in &member_ids {
            edges.push(LoopEdge {
                id: format!("kickoff->{id}"),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: id.to_string(),
                condition: LoopEdgeCondition::Always,
            });
            edges.push(LoopEdge {
                id: format!("{id}->join1"),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: id.to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            });
        }
        edges.push(LoopEdge {
            id: "join1->arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            from_node: "join1".to_string(),
            to_node: "arbiter".to_string(),
            condition: LoopEdgeCondition::Pass,
        });
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Proposers".to_string(),
            prompt_template: "draft it".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 3,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            created_at: now,
        };
        let members: Vec<EnsembleMember> = member_ids
            .iter()
            .enumerate()
            .map(|(i, id)| EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: id.to_string(),
                position: i as i64,
                platform: "openrouter".to_string(),
                model: Some(format!("model-{i}")),
                prompt_override: None,
            })
            .collect();

        db.insert_ensemble_unit(&ensemble, &members, &member_nodes, &join_node, &edges)
            .unwrap();

        // 1 call -> N+1 new graph nodes (join + 3 members), on top of the 2
        // pre-existing (kickoff, arbiter).
        let all_nodes = db.list_loop_nodes("spec-1").unwrap();
        assert_eq!(all_nodes.len(), 6);
        assert!(db.get_loop_node("join1").unwrap().is_some());
        for id in &member_ids {
            assert!(db.get_loop_node(id).unwrap().is_some());
        }

        let all_edges = db.list_loop_edges("spec-1").unwrap();
        // 2 wiring edges per member (entry + join) + 1 join exit edge.
        assert_eq!(all_edges.len(), member_ids.len() * 2 + 1);

        let details = db.get_ensemble_details("ens1").unwrap().unwrap();
        assert_eq!(details.members.len(), 3);
        // Deterministic member order (position ASC), matching insertion order.
        assert_eq!(
            details
                .members
                .iter()
                .map(|m| m.node_id.as_str())
                .collect::<Vec<_>>(),
            member_ids
        );
    }

    /// `loop_update_ensemble`'s prompt-propagation contract: updating the
    /// ensemble's shared prompt must land on the `ensembles` row (the
    /// source of truth `loop_get` reads) *and* on every member node's own
    /// `config.prompt_template` — exercising the exact two-step sequence
    /// (`update_loop_node_details` per member, then `update_ensemble_prompt`)
    /// the handler performs.
    #[test]
    fn update_ensemble_prompt_propagates_to_every_member_config() {
        let db = test_db();
        insert_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "kickoff".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true", "success_condition": "exit_code_0"}),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "arbiter".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 10,
            created_at: Utc::now(),
        })
        .unwrap();
        let now = Utc::now();
        let member_nodes = vec![
            LoopNode {
                id: "m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "member-1".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "openrouter", "prompt_template": "old prompt"}),
                position: 2,
                created_at: now,
            },
            LoopNode {
                id: "m2".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "member-2".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "claude", "prompt_template": "old prompt"}),
                position: 3,
                created_at: now,
            },
        ];
        let join_node = LoopNode {
            id: "join1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({"ensemble_id": "ens1"}),
            position: 4,
            created_at: now,
        };
        let edges = vec![
            LoopEdge {
                id: "kickoff->m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: "m1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "kickoff->m2".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: "m2".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "m1->join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "m1".to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "m2->join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "m2".to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "join1->arbiter".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "join1".to_string(),
                to_node: "arbiter".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Proposers".to_string(),
            prompt_template: "old prompt".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 2,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            created_at: now,
        };
        let members = vec![
            EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: "m1".to_string(),
                position: 0,
                platform: "openrouter".to_string(),
                model: None,
                prompt_override: None,
            },
            EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: "m2".to_string(),
                position: 1,
                platform: "claude".to_string(),
                model: None,
                prompt_override: None,
            },
        ];
        db.insert_ensemble_unit(&ensemble, &members, &member_nodes, &join_node, &edges)
            .unwrap();

        // Simulates `loop_update_ensemble`'s propagate-without-resize path:
        // rewrite every member's config with the new prompt (preserving its
        // own platform/model), then update the ensemble row itself.
        for member in &members {
            let config = serde_json::json!({
                "platform": member.platform,
                "prompt_template": "new prompt",
            });
            db.update_loop_node_details(&member.node_id, None, None, Some(&config), None)
                .unwrap();
        }
        db.update_ensemble_prompt("ens1", "new prompt").unwrap();

        assert_eq!(
            db.get_ensemble("ens1").unwrap().unwrap().prompt_template,
            "new prompt"
        );
        for member in &members {
            let node = db.get_loop_node(&member.node_id).unwrap().unwrap();
            assert_eq!(
                node.config.get("prompt_template").and_then(|v| v.as_str()),
                Some("new prompt"),
                "member '{}' must carry the propagated prompt",
                member.node_id
            );
        }
    }

    /// `min_pass`/`straggler_timeout_minutes`/`timeout_minutes` are each
    /// independently optional in an update: passing `None` for a field must
    /// leave it unchanged, and `straggler_timeout_minutes` specifically
    /// needs `Some(None)` (as opposed to bare `None`) to actually clear an
    /// override back to "defer to timeout_minutes" — this exercises both.
    #[test]
    fn update_ensemble_join_config_leaves_omitted_fields_unchanged() {
        let db = test_db();
        insert_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "kickoff".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true", "success_condition": "exit_code_0"}),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "arbiter".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 10,
            created_at: Utc::now(),
        })
        .unwrap();
        let now = Utc::now();
        let member_nodes = vec![LoopNode {
            id: "m1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "member-1".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "openrouter"}),
            position: 2,
            created_at: now,
        }];
        let join_node = LoopNode {
            id: "join1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({"ensemble_id": "ens1"}),
            position: 3,
            created_at: now,
        };
        let edges = vec![
            LoopEdge {
                id: "kickoff->m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: "m1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "m1->join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "m1".to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "join1->arbiter".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "join1".to_string(),
                to_node: "arbiter".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Solo".to_string(),
            prompt_template: "draft it".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 1,
            straggler_timeout_minutes: Some(15),
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            created_at: now,
        };
        let members = vec![EnsembleMember {
            ensemble_id: "ens1".to_string(),
            node_id: "m1".to_string(),
            position: 0,
            platform: "openrouter".to_string(),
            model: None,
            prompt_override: None,
        }];
        db.insert_ensemble_unit(&ensemble, &members, &member_nodes, &join_node, &edges)
            .unwrap();

        // Only min_pass changes; straggler_timeout_minutes/timeout_minutes omitted.
        db.update_ensemble_join_config("ens1", Some(1), None, None)
            .unwrap();
        let after = db.get_ensemble("ens1").unwrap().unwrap();
        assert_eq!(after.min_pass, 1);
        assert_eq!(after.straggler_timeout_minutes, Some(15));
        assert_eq!(after.timeout_minutes, 30);

        // Explicitly clear straggler_timeout_minutes back to "defer to timeout_minutes".
        db.update_ensemble_join_config("ens1", None, Some(None), None)
            .unwrap();
        let cleared = db.get_ensemble("ens1").unwrap().unwrap();
        assert_eq!(cleared.straggler_timeout_minutes, None);
        assert_eq!(
            cleared.effective_straggler_timeout_minutes(),
            cleared.timeout_minutes
        );
    }

    /// Removing a member cascades away its node, its `ensemble_members` row,
    /// and both of its wiring edges (entry + join) — the rest of the
    /// ensemble is left intact.
    #[test]
    fn remove_ensemble_member_cascades_node_and_edges() {
        let db = test_db();
        insert_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "kickoff".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true", "success_condition": "exit_code_0"}),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "arbiter".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 10,
            created_at: Utc::now(),
        })
        .unwrap();
        let now = Utc::now();
        let member_ids = ["m1", "m2"];
        let member_nodes: Vec<LoopNode> = member_ids
            .iter()
            .enumerate()
            .map(|(i, id)| LoopNode {
                id: id.to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: format!("member-{}", i + 1),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "openrouter"}),
                position: 2 + i as i64,
                created_at: now,
            })
            .collect();
        let join_node = LoopNode {
            id: "join1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({"ensemble_id": "ens1"}),
            position: 4,
            created_at: now,
        };
        let mut edges = Vec::new();
        for id in &member_ids {
            edges.push(LoopEdge {
                id: format!("kickoff->{id}"),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: id.to_string(),
                condition: LoopEdgeCondition::Always,
            });
            edges.push(LoopEdge {
                id: format!("{id}->join1"),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: id.to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            });
        }
        edges.push(LoopEdge {
            id: "join1->arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            from_node: "join1".to_string(),
            to_node: "arbiter".to_string(),
            condition: LoopEdgeCondition::Pass,
        });
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Proposers".to_string(),
            prompt_template: "draft it".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 2,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            created_at: now,
        };
        let members: Vec<EnsembleMember> = member_ids
            .iter()
            .enumerate()
            .map(|(i, id)| EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: id.to_string(),
                position: i as i64,
                platform: "openrouter".to_string(),
                model: None,
                prompt_override: None,
            })
            .collect();
        db.insert_ensemble_unit(&ensemble, &members, &member_nodes, &join_node, &edges)
            .unwrap();

        assert!(db.remove_ensemble_member("m2").unwrap());

        assert!(db.get_loop_node("m2").unwrap().is_none());
        let remaining_edges = db.list_loop_edges("spec-1").unwrap();
        assert!(!remaining_edges
            .iter()
            .any(|edge| edge.from_node == "m2" || edge.to_node == "m2"));
        let details = db.get_ensemble_details("ens1").unwrap().unwrap();
        assert_eq!(
            details
                .members
                .iter()
                .map(|m| m.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m1"]
        );
        // The surviving member and the join itself are unaffected.
        assert!(db.get_loop_node("m1").unwrap().is_some());
        assert!(db.get_loop_node("join1").unwrap().is_some());
    }

    #[test]
    fn get_ensemble_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let result = db.get_ensemble("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_ensemble_members_empty() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let members = db.list_ensemble_members("nonexistent").unwrap();
        assert!(members.is_empty());
    }

    #[test]
    fn get_ensemble_details_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let result = db.get_ensemble_details("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_ensembles_for_spec_empty() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let ensembles = db.list_ensembles_for_spec("nonexistent").unwrap();
        assert!(ensembles.is_empty());
    }

    #[test]
    fn list_ensembles_for_loop_empty() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let ensembles = db.list_ensembles_for_loop("nonexistent").unwrap();
        assert!(ensembles.is_empty());
    }

    #[test]
    fn get_ensemble_by_member_node_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let result = db.get_ensemble_by_member_node("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn get_ensemble_by_join_node_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let result = db.get_ensemble_by_join_node("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn update_ensemble_prompt_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let updated = db
            .update_ensemble_prompt("nonexistent", "new prompt")
            .unwrap();
        assert!(!updated);
    }

    #[test]
    fn remove_ensemble_member_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let removed = db.remove_ensemble_member("nonexistent").unwrap();
        assert!(!removed);
    }

    #[test]
    fn get_ensemble_blueprint_by_name_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let result = db.get_ensemble_blueprint_by_name("nonexistent").unwrap();
        assert!(result.is_none());
    }

    /// An installation whose `ensemble_blueprints` row still carries the old
    /// hardcoded free-tier model identities must have them reconciled away
    /// (to `None`) on the next startup reseed, and the reseed must be
    /// idempotent.
    #[test]
    fn seed_builtin_ensemble_blueprints_migrates_hardcoded_model_identities() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        db.delete_ensemble_blueprint_by_name("ensemble-proposers")
            .unwrap();
        db.insert_ensemble_blueprint(&EnsembleBlueprint {
            id: uuid::Uuid::new_v4().to_string(),
            name: "ensemble-proposers".to_string(),
            prompt_template: "Draft it".to_string(),
            members: vec![
                (
                    "openrouter".to_string(),
                    Some("deepseek/deepseek-chat-v3.1:free".to_string()),
                    None,
                ),
                (
                    "openrouter".to_string(),
                    Some("qwen/qwen3-coder:free".to_string()),
                    None,
                ),
                (
                    "openrouter".to_string(),
                    Some("meta-llama/llama-3.3-70b-instruct:free".to_string()),
                    None,
                ),
            ],
            min_pass: None,
            builtin: true,
            created_at: Utc::now(),
        })
        .unwrap();

        db.seed_builtin_ensemble_blueprints().unwrap();

        let migrated = db
            .get_ensemble_blueprint_by_name("ensemble-proposers")
            .unwrap()
            .unwrap();
        assert_eq!(migrated.members.len(), 3);
        for (platform, model, _) in &migrated.members {
            assert_eq!(platform, "openrouter");
            assert!(model.is_none(), "model identity must be reconciled away");
        }

        // Idempotent: a second reseed changes nothing further.
        db.seed_builtin_ensemble_blueprints().unwrap();
        let after_second = db
            .get_ensemble_blueprint_by_name("ensemble-proposers")
            .unwrap()
            .unwrap();
        assert_eq!(after_second.members, migrated.members);
    }

    /// A custom ensemble blueprint that claims the builtin's name must never
    /// be overwritten or deleted by the migration.
    #[test]
    fn seed_builtin_ensemble_blueprints_never_touches_a_custom_blueprint_with_a_builtin_name() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.delete_ensemble_blueprint_by_name("ensemble-proposers")
            .unwrap();

        let custom = EnsembleBlueprint {
            id: uuid::Uuid::new_v4().to_string(),
            name: "ensemble-proposers".to_string(),
            prompt_template: "my own take".to_string(),
            members: vec![("claude".to_string(), None, None)],
            min_pass: Some(1),
            builtin: false,
            created_at: Utc::now(),
        };
        db.insert_ensemble_blueprint(&custom).unwrap();

        db.seed_builtin_ensemble_blueprints().unwrap();

        let fetched = db
            .get_ensemble_blueprint_by_name("ensemble-proposers")
            .unwrap()
            .unwrap();
        assert!(
            !fetched.builtin,
            "custom ensemble blueprint must stay custom"
        );
        assert_eq!(fetched.prompt_template, "my own take");
    }

    /// A member's `prompt_override` round-trips through `insert_ensemble_unit`
    /// and `get_ensemble_details` — `Some` for a member that has one, `None`
    /// for a member that doesn't (using the shared `prompt_template` exactly
    /// as every ensemble did before this field existed).
    #[test]
    fn insert_ensemble_unit_round_trips_member_prompt_override() {
        let db = test_db();
        insert_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "kickoff".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true", "success_condition": "exit_code_0"}),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "arbiter".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 10,
            created_at: Utc::now(),
        })
        .unwrap();
        let now = Utc::now();
        let member_nodes = vec![
            LoopNode {
                id: "m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "member-1".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "claude", "prompt_template": "review for security"}),
                position: 2,
                created_at: now,
            },
            LoopNode {
                id: "m2".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "member-2".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "claude", "prompt_template": "draft it"}),
                position: 3,
                created_at: now,
            },
        ];
        let join_node = LoopNode {
            id: "join1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({"ensemble_id": "ens1"}),
            position: 4,
            created_at: now,
        };
        let edges = vec![
            LoopEdge {
                id: "kickoff->m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: "m1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "kickoff->m2".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: "m2".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "m1->join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "m1".to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "m2->join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "m2".to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "join1->arbiter".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "join1".to_string(),
                to_node: "arbiter".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Proposers".to_string(),
            prompt_template: "draft it".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 2,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            created_at: now,
        };
        let members = vec![
            EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: "m1".to_string(),
                position: 0,
                platform: "claude".to_string(),
                model: None,
                prompt_override: Some("review for security".to_string()),
            },
            EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: "m2".to_string(),
                position: 1,
                platform: "claude".to_string(),
                model: None,
                prompt_override: None,
            },
        ];
        db.insert_ensemble_unit(&ensemble, &members, &member_nodes, &join_node, &edges)
            .unwrap();

        let details = db.get_ensemble_details("ens1").unwrap().unwrap();
        assert_eq!(
            details.members[0].prompt_override.as_deref(),
            Some("review for security")
        );
        assert_eq!(details.members[1].prompt_override, None);
    }

    /// `update_ensemble_member` (the renamed `update_ensemble_member_platform`)
    /// sets `prompt_override` outright, in both directions: giving a member
    /// that had none its own override, and clearing an existing override back
    /// to `None` (the "use the shared prompt" state) — both are ordinary
    /// `Some`/`None` writes, never a "leave unchanged" skip, since a
    /// replacement member list is always given in full.
    #[test]
    fn update_ensemble_member_sets_and_clears_prompt_override() {
        let db = test_db();
        insert_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "kickoff".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true", "success_condition": "exit_code_0"}),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "arbiter".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 10,
            created_at: Utc::now(),
        })
        .unwrap();
        let now = Utc::now();
        let member_nodes = vec![LoopNode {
            id: "m1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "member-1".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude", "prompt_template": "draft it"}),
            position: 2,
            created_at: now,
        }];
        let join_node = LoopNode {
            id: "join1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({"ensemble_id": "ens1"}),
            position: 3,
            created_at: now,
        };
        let edges = vec![
            LoopEdge {
                id: "kickoff->m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: "m1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "m1->join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "m1".to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "join1->arbiter".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "join1".to_string(),
                to_node: "arbiter".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Solo".to_string(),
            prompt_template: "draft it".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 1,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            created_at: now,
        };
        let members = vec![EnsembleMember {
            ensemble_id: "ens1".to_string(),
            node_id: "m1".to_string(),
            position: 0,
            platform: "claude".to_string(),
            model: None,
            prompt_override: None,
        }];
        db.insert_ensemble_unit(&ensemble, &members, &member_nodes, &join_node, &edges)
            .unwrap();

        db.update_ensemble_member("ens1", "m1", "claude", None, Some("new angle"))
            .unwrap();
        let after_set = db.get_ensemble_details("ens1").unwrap().unwrap();
        assert_eq!(
            after_set.members[0].prompt_override.as_deref(),
            Some("new angle")
        );

        db.update_ensemble_member("ens1", "m1", "claude", None, None)
            .unwrap();
        let after_clear = db.get_ensemble_details("ens1").unwrap().unwrap();
        assert_eq!(after_clear.members[0].prompt_override, None);
    }

    /// A blueprint row seeded before `prompt_override` existed stores its
    /// `members` JSON as 2-element `[platform, model]` arrays — an upgrade
    /// must still be able to read that row back (as "no override for any
    /// member") without a data migration, alongside the current 3-element
    /// shape.
    #[test]
    fn parse_ensemble_blueprint_members_tolerates_legacy_two_element_rows() {
        let legacy = r#"[["openrouter","deepseek/deepseek-chat-v3.1:free"],["claude",null]]"#;
        let parsed = parse_ensemble_blueprint_members(legacy).unwrap();
        assert_eq!(
            parsed,
            vec![
                (
                    "openrouter".to_string(),
                    Some("deepseek/deepseek-chat-v3.1:free".to_string()),
                    None
                ),
                ("claude".to_string(), None, None),
            ]
        );

        let current = r#"[["openrouter","deepseek/deepseek-chat-v3.1:free","review it"],["claude",null,null]]"#;
        let parsed = parse_ensemble_blueprint_members(current).unwrap();
        assert_eq!(
            parsed,
            vec![
                (
                    "openrouter".to_string(),
                    Some("deepseek/deepseek-chat-v3.1:free".to_string()),
                    Some("review it".to_string())
                ),
                ("claude".to_string(), None, None),
            ]
        );
    }
}
