use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::io::{Error as IoError, ErrorKind};

use crate::db::Database;
use crate::domain::workflow::{
    Workflow, WorkflowDetails, WorkflowEdge, WorkflowEdgeCondition, WorkflowNode, WorkflowNodeKind,
    WorkflowNodeRun, WorkflowRunStatus, WorkflowSpec, WorkflowSpecDetails, WorkflowSpecStatus,
    WorkflowStatus,
};

impl Database {
    pub fn insert_workflow(&self, workflow: &Workflow) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO workflows (id, name, description, workdir, status, created_at, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &workflow.id,
                &workflow.name,
                &workflow.description,
                &workflow.workdir,
                workflow.status.as_str(),
                workflow.created_at.timestamp(),
                workflow.started_at.map(|value| value.timestamp()),
                workflow.completed_at.map(|value| value.timestamp()),
            ],
        )?;
        Ok(())
    }

    pub fn get_workflow(&self, workflow_id: &str) -> Result<Option<Workflow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, description, workdir, status, created_at, started_at, completed_at
             FROM workflows WHERE id = ?1",
        )?;

        stmt.query_row(params![workflow_id], map_workflow_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn list_workflows(&self, workdir: Option<&str>) -> Result<Vec<Workflow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let sql = if workdir.is_some() {
            "SELECT id, name, description, workdir, status, created_at, started_at, completed_at
             FROM workflows WHERE workdir = ?1 ORDER BY created_at DESC"
        } else {
            "SELECT id, name, description, workdir, status, created_at, started_at, completed_at
             FROM workflows ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(workdir) = workdir {
            stmt.query_map(params![workdir], map_workflow_row)?
        } else {
            stmt.query_map([], map_workflow_row)?
        };

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_workflow_status(
        &self,
        workflow_id: &str,
        status: WorkflowStatus,
        started_at: Option<DateTime<Utc>>,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE workflows
             SET status = ?1,
                 started_at = COALESCE(?2, started_at),
                 completed_at = COALESCE(?3, completed_at)
             WHERE id = ?4",
            params![
                status.as_str(),
                started_at.map(|value| value.timestamp()),
                completed_at.map(|value| value.timestamp()),
                workflow_id,
            ],
        )?;
        Ok(rows > 0)
    }

    pub fn insert_workflow_spec(&self, spec: &WorkflowSpec) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO workflow_specs (id, workflow_id, name, description, position, parallelizable, status, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &spec.id,
                &spec.workflow_id,
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

    pub fn list_workflow_specs(&self, workflow_id: &str) -> Result<Vec<WorkflowSpec>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, workflow_id, name, description, position, parallelizable, status, started_at, completed_at
             FROM workflow_specs WHERE workflow_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![workflow_id], map_workflow_spec_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_workflow_spec(&self, spec_id: &str) -> Result<Option<WorkflowSpec>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, workflow_id, name, description, position, parallelizable, status, started_at, completed_at
             FROM workflow_specs WHERE id = ?1",
        )?;

        stmt.query_row(params![spec_id], map_workflow_spec_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn update_workflow_spec_status(
        &self,
        spec_id: &str,
        status: WorkflowSpecStatus,
        started_at: Option<DateTime<Utc>>,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute(
            "UPDATE workflow_specs
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

    pub fn insert_workflow_node(&self, node: &WorkflowNode) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO workflow_nodes (id, spec_id, name, kind, config, position, created_at)
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

    pub fn list_workflow_nodes(&self, spec_id: &str) -> Result<Vec<WorkflowNode>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, name, kind, config, position, created_at
             FROM workflow_nodes WHERE spec_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_workflow_node_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn insert_workflow_edge(&self, edge: &WorkflowEdge) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO workflow_edges (id, spec_id, from_node, to_node, condition)
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

    pub fn list_workflow_edges(&self, spec_id: &str) -> Result<Vec<WorkflowEdge>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, spec_id, from_node, to_node, condition
             FROM workflow_edges WHERE spec_id = ?1 ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_workflow_edge_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn insert_workflow_run(&self, run: &WorkflowNodeRun) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO workflow_runs (id, workflow_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &run.id,
                &run.workflow_id,
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

    pub fn list_workflow_runs_for_spec(&self, spec_id: &str) -> Result<Vec<WorkflowNodeRun>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, workflow_id, spec_id, node_id, status, input, output, started_at, completed_at, iteration
             FROM workflow_runs WHERE spec_id = ?1 ORDER BY started_at ASC, iteration ASC",
        )?;
        let rows = stmt.query_map(params![spec_id], map_workflow_run_row)?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_workflow_details(&self, workflow_id: &str) -> Result<Option<WorkflowDetails>> {
        let Some(workflow) = self.get_workflow(workflow_id)? else {
            return Ok(None);
        };
        let specs = self
            .list_workflow_specs(workflow_id)?
            .into_iter()
            .map(|spec| {
                let nodes = self.list_workflow_nodes(&spec.id)?;
                let edges = self.list_workflow_edges(&spec.id)?;
                Ok(WorkflowSpecDetails { spec, nodes, edges })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Some(WorkflowDetails { workflow, specs }))
    }
}

fn map_workflow_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workflow> {
    Ok(Workflow {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        workdir: row.get(3)?,
        status: WorkflowStatus::from_str(&row.get::<_, String>(4)?),
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

fn map_workflow_spec_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowSpec> {
    Ok(WorkflowSpec {
        id: row.get(0)?,
        workflow_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        position: row.get(4)?,
        parallelizable: row.get(5)?,
        status: WorkflowSpecStatus::from_str(&row.get::<_, String>(6)?),
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

fn map_workflow_node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowNode> {
    let kind = WorkflowNodeKind::from_str(&row.get::<_, String>(3)?).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(IoError::new(
                ErrorKind::InvalidData,
                "Invalid workflow node kind",
            )),
        )
    })?;
    let config_raw: String = row.get(4)?;
    let config = parse_json_value(&config_raw)?;

    Ok(WorkflowNode {
        id: row.get(0)?,
        spec_id: row.get(1)?,
        name: row.get(2)?,
        kind,
        config,
        position: row.get(5)?,
        created_at: from_timestamp(row.get(6)?)?,
    })
}

fn map_workflow_edge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowEdge> {
    let condition =
        WorkflowEdgeCondition::from_str(&row.get::<_, String>(4)?).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(IoError::new(
                    ErrorKind::InvalidData,
                    "Invalid workflow edge condition",
                )),
            )
        })?;

    Ok(WorkflowEdge {
        id: row.get(0)?,
        spec_id: row.get(1)?,
        from_node: row.get(2)?,
        to_node: row.get(3)?,
        condition,
    })
}

fn map_workflow_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowNodeRun> {
    Ok(WorkflowNodeRun {
        id: row.get(0)?,
        workflow_id: row.get(1)?,
        spec_id: row.get(2)?,
        node_id: row.get(3)?,
        status: WorkflowRunStatus::from_str(&row.get::<_, String>(4)?),
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
