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

    #[test]
    fn count_intelligence_nodes_with_data() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO intelligence_nodes (id, kind, title, body, created_at, updated_at) VALUES (?1, 'fact', 'Test', 'Body', ?2, ?2)",
            rusqlite::params!["node1", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO intelligence_nodes (id, kind, title, body, created_at, updated_at) VALUES (?1, 'pattern', 'Test2', 'Body2', ?2, ?2)",
            rusqlite::params!["node2", now],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.count_intelligence_nodes().unwrap(), 2);
    }

    #[test]
    fn count_cross_project_intelligence_links_with_data() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO intelligence_nodes (id, kind, title, body, project_hash, created_at, updated_at) VALUES (?1, 'fact', 'N1', 'B1', ?2, ?3, ?3)",
            rusqlite::params!["n1", "hash_a", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO intelligence_nodes (id, kind, title, body, project_hash, created_at, updated_at) VALUES (?1, 'fact', 'N2', 'B2', ?2, ?3, ?3)",
            rusqlite::params!["n2", "hash_b", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO intelligence_edges (from_node_id, to_node_id, relation, created_at) VALUES (?1, ?2, 'relates_to', ?3)",
            rusqlite::params!["n1", "n2", now],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.count_cross_project_intelligence_links().unwrap(), 1);
    }

    #[test]
    fn count_cross_project_same_project_not_counted() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO intelligence_nodes (id, kind, title, body, project_hash, created_at, updated_at) VALUES (?1, 'fact', 'N1', 'B1', ?2, ?3, ?3)",
            rusqlite::params!["n1", "hash_a", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO intelligence_nodes (id, kind, title, body, project_hash, created_at, updated_at) VALUES (?1, 'fact', 'N2', 'B2', ?2, ?3, ?3)",
            rusqlite::params!["n2", "hash_a", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO intelligence_edges (from_node_id, to_node_id, relation, created_at) VALUES (?1, ?2, 'relates_to', ?3)",
            rusqlite::params!["n1", "n2", now],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.count_cross_project_intelligence_links().unwrap(), 0);
    }

    #[test]
    fn count_loop_node_runs_with_data() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // Insert a loop
        conn.execute(
            "INSERT INTO loops (id, name, workdir, status, created_at) VALUES (?1, 'test', '/tmp', 'draft', ?2)",
            rusqlite::params!["loop1", now],
        )
        .unwrap();
        // Insert a spec
        conn.execute(
            "INSERT INTO loop_specs (id, loop_id, name, position, parallelizable, status) VALUES (?1, ?2, 'spec1', 0, 0, 'pending')",
            rusqlite::params!["spec1", "loop1"],
        )
        .unwrap();
        // Insert a node (spec_id non-null, loop_id null)
        conn.execute(
            "INSERT INTO loop_nodes (id, spec_id, loop_id, name, kind, config, position, created_at) VALUES (?1, ?2, NULL, 'node1', 'agent', '{}', 0, ?3)",
            rusqlite::params!["n1", "spec1", now],
        )
        .unwrap();
        // Insert a loop_run
        conn.execute(
            "INSERT INTO loop_runs (id, loop_id, spec_id, node_id, status, started_at, iteration) VALUES (?1, ?2, ?3, ?4, 'success', ?5, 1)",
            rusqlite::params!["run1", "loop1", "spec1", "n1", now],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.count_loop_node_runs().unwrap(), 1);
    }

    #[test]
    fn count_completed_loops_with_data() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO loops (id, name, workdir, status, created_at) VALUES (?1, 'completed', '/tmp', 'completed', ?2)",
            rusqlite::params!["loop1", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loops (id, name, workdir, status, created_at) VALUES (?1, 'running', '/tmp', 'running', ?2)",
            rusqlite::params!["loop2", now],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.count_completed_loops().unwrap(), 1);
    }

    #[test]
    fn max_loop_nodes_in_any_loop_with_data() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // Loop with 1 spec, 2 nodes
        conn.execute(
            "INSERT INTO loops (id, name, workdir, status, created_at) VALUES (?1, 'loop1', '/tmp', 'draft', ?2)",
            rusqlite::params!["loop1", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_specs (id, loop_id, name, position, parallelizable, status) VALUES (?1, ?2, 'spec1', 0, 0, 'pending')",
            rusqlite::params!["spec1", "loop1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_nodes (id, spec_id, loop_id, name, kind, config, position, created_at) VALUES (?1, ?2, NULL, 'n1', 'agent', '{}', 0, ?3)",
            rusqlite::params!["n1", "spec1", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_nodes (id, spec_id, loop_id, name, kind, config, position, created_at) VALUES (?1, ?2, NULL, 'n2', 'check', '{}', 1, ?3)",
            rusqlite::params!["n2", "spec1", now],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.max_loop_nodes_in_any_loop().unwrap(), 2);
    }

    #[test]
    fn has_parallel_loop_run_false_when_non_parallel() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO loops (id, name, workdir, status, created_at) VALUES (?1, 'loop1', '/tmp', 'draft', ?2)",
            rusqlite::params!["loop1", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_specs (id, loop_id, name, position, parallelizable, status) VALUES (?1, ?2, 'spec1', 0, 0, 'pending')",
            rusqlite::params!["spec1", "loop1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_nodes (id, spec_id, loop_id, name, kind, config, position, created_at) VALUES (?1, ?2, NULL, 'n1', 'agent', '{}', 0, ?3)",
            rusqlite::params!["n1", "spec1", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_runs (id, loop_id, spec_id, node_id, status, started_at, iteration) VALUES (?1, ?2, ?3, ?4, 'success', ?5, 1)",
            rusqlite::params!["run1", "loop1", "spec1", "n1", now],
        )
        .unwrap();
        drop(conn);
        assert!(!db.has_parallel_loop_run().unwrap());
    }

    #[test]
    fn has_parallel_loop_run_true_when_parallel() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO loops (id, name, workdir, status, created_at) VALUES (?1, 'loop1', '/tmp', 'draft', ?2)",
            rusqlite::params!["loop1", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_specs (id, loop_id, name, position, parallelizable, status) VALUES (?1, ?2, 'spec1', 0, 1, 'pending')",
            rusqlite::params!["spec1", "loop1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_nodes (id, spec_id, loop_id, name, kind, config, position, created_at) VALUES (?1, ?2, NULL, 'n1', 'agent', '{}', 0, ?3)",
            rusqlite::params!["n1", "spec1", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO loop_runs (id, loop_id, spec_id, node_id, status, started_at, iteration) VALUES (?1, ?2, ?3, ?4, 'success', ?5, 1)",
            rusqlite::params!["run1", "loop1", "spec1", "n1", now],
        )
        .unwrap();
        drop(conn);
        assert!(db.has_parallel_loop_run().unwrap());
    }

    #[test]
    fn max_seed_session_bindings_with_data() {
        let db = test_db();
        // Need an interactive_sessions row for the FK
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO interactive_sessions (id, name, cli, working_dir, started_at) VALUES (?1, 's1', 'opencode', '/tmp', '2024-01-01T00:00:00Z')",
            rusqlite::params!["sess1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO interactive_sessions (id, name, cli, working_dir, started_at) VALUES (?1, 's2', 'opencode', '/tmp', '2024-01-01T00:00:00Z')",
            rusqlite::params!["sess2"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO seed_sessions (session_id, seed_id, bound_at) VALUES (?1, 'seed1', '2024-01-01T00:00:00Z')",
            rusqlite::params!["sess1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO seed_sessions (session_id, seed_id, bound_at) VALUES (?1, 'seed1', '2024-01-01T00:00:00Z')",
            rusqlite::params!["sess2"],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.max_seed_session_bindings().unwrap(), 2);
    }
}
