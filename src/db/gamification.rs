//! Database queries supporting gamification mission checks.

use anyhow::Result;

use crate::db::Database;

impl Database {
    pub fn count_intelligence_nodes(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM intelligence_nodes", [], |r| r.get(0))?;
        Ok(count)
    }

    pub fn count_cross_project_intelligence_links(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM intelligence_edges e
             JOIN intelligence_nodes fn ON fn.id = e.from_node_id
             JOIN intelligence_nodes tn ON tn.id = e.to_node_id
             WHERE fn.project_hash IS NOT NULL
               AND tn.project_hash IS NOT NULL
               AND fn.project_hash != tn.project_hash",
            [],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    pub fn count_workflow_node_runs(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM workflow_runs", [], |r| r.get(0))?;
        Ok(count)
    }

    pub fn count_completed_workflows(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM workflows WHERE status = 'completed'",
            [],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    pub fn max_workflow_nodes_in_any_workflow(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let max: i64 = conn.query_row(
            "SELECT COALESCE(MAX(node_count), 0) FROM (
                SELECT ws.workflow_id, COUNT(wn.id) AS node_count
                FROM workflow_specs ws
                JOIN workflow_nodes wn ON wn.spec_id = ws.id
                GROUP BY ws.workflow_id
             )",
            [],
            |r| r.get(0),
        )?;
        Ok(max.max(0) as usize)
    }

    pub fn has_parallel_workflow_run(&self) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM workflow_runs wr
             JOIN workflow_specs ws ON ws.id = wr.spec_id
             WHERE ws.parallelizable = 1",
            [],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn max_seed_session_bindings(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let max: i64 = conn.query_row(
            "SELECT COALESCE(MAX(cnt), 0) FROM (
                SELECT COUNT(*) AS cnt FROM seed_sessions GROUP BY seed_id
             )",
            [],
            |r| r.get(0),
        )?;
        Ok(max)
    }
}
