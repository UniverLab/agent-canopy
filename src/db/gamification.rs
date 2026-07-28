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

    pub fn count_loop_node_runs(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM loop_runs", [], |r| r.get(0))?;
        Ok(count)
    }

    pub fn count_completed_loops(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM loops WHERE status = 'completed'",
            [],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    pub fn max_loop_nodes_in_any_loop(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let max: i64 = conn.query_row(
            "SELECT COALESCE(MAX(node_count), 0) FROM (
                SELECT ws.loop_id, COUNT(wn.id) AS node_count
                FROM loop_specs ws
                JOIN loop_nodes wn ON wn.spec_id = ws.id
                GROUP BY ws.loop_id
             )",
            [],
            |r| r.get(0),
        )?;
        Ok(max.max(0) as usize)
    }

    pub fn has_parallel_loop_run(&self) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM loop_runs wr
             JOIN loop_specs ws ON ws.id = wr.spec_id
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    #[test]
    fn count_intelligence_nodes_empty() {
        let db = test_db();
        let count = db.count_intelligence_nodes().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn count_cross_project_intelligence_links_empty() {
        let db = test_db();
        let count = db.count_cross_project_intelligence_links().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn count_loop_node_runs_empty() {
        let db = test_db();
        let count = db.count_loop_node_runs().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn count_completed_loops_empty() {
        let db = test_db();
        let count = db.count_completed_loops().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn max_loop_nodes_in_any_loop_empty() {
        let db = test_db();
        let max = db.max_loop_nodes_in_any_loop().unwrap();
        assert_eq!(max, 0);
    }

    #[test]
    fn max_seed_session_bindings_empty() {
        let db = test_db();
        let max = db.max_seed_session_bindings().unwrap();
        assert_eq!(max, 0);
    }
}
