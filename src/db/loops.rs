use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::io::{Error as IoError, ErrorKind};

use crate::db::Database;
use crate::domain::loops::{
    Loop, LoopDetails, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind, LoopNodeRun,
    LoopRunStatus, LoopSpec, LoopSpecDetails, LoopSpecStatus, LoopStatus,
};

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
        conn.execute(
            "INSERT INTO loops (id, name, description, workdir, status, created_at, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &lp.id,
                &lp.name,
                &lp.description,
                &lp.workdir,
                lp.status.as_str(),
                lp.created_at.timestamp(),
                lp.started_at.map(|value| value.timestamp()),
                lp.completed_at.map(|value| value.timestamp()),
            ],
        )?;
        Ok(())
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
            "SELECT id, name, description, workdir, status, created_at, started_at, completed_at
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
            "SELECT id, name, description, workdir, status, created_at, started_at, completed_at
             FROM loops WHERE workdir = ?1 ORDER BY created_at DESC"
        } else {
            "SELECT id, name, description, workdir, status, created_at, started_at, completed_at
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

    pub fn insert_loop_spec(&self, spec: &LoopSpec) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO loop_specs (id, loop_id, name, description, position, parallelizable, status, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
            "SELECT id, loop_id, name, description, position, parallelizable, status, started_at, completed_at
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
            "SELECT id, loop_id, name, description, position, parallelizable, status, started_at, completed_at
             FROM loop_specs WHERE id = ?1",
        )?;

        stmt.query_row(params![spec_id], map_loop_spec_row)
            .optional()
            .map_err(Into::into)
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
    Ok(Loop {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        workdir: row.get(3)?,
        status: LoopStatus::from_str(&row.get::<_, String>(4)?),
        created_at: from_timestamp(row.get(5)?)?,
        started_at: row
            .get::<_, Option<i64>>(6)?
            .map(from_timestamp)
            .transpose()?,
        completed_at: row
            .get::<_, Option<i64>>(7)?
            .map(from_timestamp)
            .transpose()?,
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
