use anyhow::Result;
use rusqlite::params;

use crate::db::Database;
use crate::domain::sandbox::Sandbox;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SandboxRun {
    pub id: String,
    pub project_hash: String,
    pub base_branch: String,
    pub sandbox_branch: String,
    pub worktree_path: String,
    pub cli_name: String,
    pub original_workdir: String,
    pub owner_type: String,
    pub owner_id: String,
    pub created_at: String,
    pub status: String,
    /// CB42: cleanup failure detail, set when end-of-run teardown could not
    /// remove the worktree/branch (or could not prove it safe to do so).
    pub cleanup_error: Option<String>,
}

const SANDBOX_RUN_COLUMNS: &str = "id, project_hash, base_branch, sandbox_branch, worktree_path, cli_name, original_workdir, owner_type, owner_id, created_at, status, cleanup_error";

fn sandbox_run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SandboxRun> {
    Ok(SandboxRun {
        id: row.get(0)?,
        project_hash: row.get(1)?,
        base_branch: row.get(2)?,
        sandbox_branch: row.get(3)?,
        worktree_path: row.get(4)?,
        cli_name: row.get(5)?,
        original_workdir: row.get(6)?,
        owner_type: row.get(7)?,
        owner_id: row.get(8)?,
        created_at: row.get(9)?,
        status: row.get(10)?,
        cleanup_error: row.get(11)?,
    })
}

#[allow(dead_code)]
impl Database {
    pub fn insert_sandbox_run(
        &self,
        sandbox: &Sandbox,
        owner_type: &str,
        owner_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "INSERT INTO sandbox_runs (id, project_hash, base_branch, sandbox_branch, worktree_path, cli_name, original_workdir, owner_type, owner_id, created_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'active')",
            params![
                sandbox.id,
                sandbox.project_hash,
                sandbox.base_branch,
                sandbox.sandbox_branch,
                sandbox.worktree_path.to_string_lossy(),
                sandbox.cli_name,
                sandbox.original_workdir,
                owner_type,
                owner_id,
                sandbox.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn update_sandbox_run_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE sandbox_runs SET status = ?1 WHERE id = ?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn get_sandbox_run(&self, id: &str) -> Result<Option<SandboxRun>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {SANDBOX_RUN_COLUMNS} FROM sandbox_runs WHERE id = ?1",
        ))?;
        let result = stmt
            .query_row(params![id], sandbox_run_from_row)
            .optional()?;
        Ok(result)
    }

    /// Reconstruct the live [`Sandbox`] for a still-`active` run, so a
    /// resumed loop (or a reopened session) keeps operating in its worktree
    /// instead of silently falling back to the user's real checkout.
    pub fn get_active_sandbox_for_owner(
        &self,
        owner_type: &str,
        owner_id: &str,
    ) -> Result<Option<Sandbox>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let row = conn
            .query_row(
                "SELECT id, project_hash, base_branch, sandbox_branch, worktree_path, cli_name, original_workdir, created_at
                 FROM sandbox_runs
                 WHERE owner_type = ?1 AND owner_id = ?2 AND status = 'active'
                 ORDER BY created_at DESC LIMIT 1",
                params![owner_type, owner_id],
                |row| {
                    Ok(Sandbox {
                        id: row.get(0)?,
                        project_hash: row.get(1)?,
                        base_branch: row.get(2)?,
                        sandbox_branch: row.get(3)?,
                        worktree_path: std::path::PathBuf::from(row.get::<_, String>(4)?),
                        cli_name: row.get(5)?,
                        original_workdir: row.get(6)?,
                        created_at: row
                            .get::<_, String>(7)?
                            .parse::<chrono::DateTime<chrono::Utc>>()
                            .unwrap_or_else(|_| chrono::Utc::now()),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_active_sandbox_runs(&self) -> Result<Vec<SandboxRun>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {SANDBOX_RUN_COLUMNS} FROM sandbox_runs WHERE status = 'active' ORDER BY created_at ASC",
        ))?;
        let rows = stmt.query_map([], sandbox_run_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Every sandbox run, oldest first — the input to `canopy sandbox list`.
    pub fn list_all_sandbox_runs(&self) -> Result<Vec<SandboxRun>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {SANDBOX_RUN_COLUMNS} FROM sandbox_runs ORDER BY created_at ASC",
        ))?;
        let rows = stmt.query_map([], sandbox_run_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Runs no longer in flight: everything except `active`/`merging`
    /// (kept in sync with the statuses `insert_sandbox_run` writes
    /// (`active`) and the engine writes). The input to `canopy clean`'s
    /// bulk sandbox path.
    pub fn list_finished_sandbox_runs(&self) -> Result<Vec<SandboxRun>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {SANDBOX_RUN_COLUMNS} FROM sandbox_runs \
             WHERE status IN ('merged','failed','kept','cleaned','cleanup_failed','discarded') \
             ORDER BY created_at ASC",
        ))?;
        let rows = stmt.query_map([], sandbox_run_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Record why end-of-run teardown could not reclaim this sandbox.
    pub fn set_sandbox_cleanup_error(&self, id: &str, msg: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.execute(
            "UPDATE sandbox_runs SET cleanup_error = ?1 WHERE id = ?2",
            params![msg, id],
        )?;
        Ok(())
    }

    /// Resolve a sandbox id from an exact id or an unambiguous prefix.
    /// Returns `Ok(None)` when nothing matches; errors when the prefix is
    /// ambiguous (naming the candidates).
    pub fn resolve_sandbox_id_by_prefix(&self, prefix: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        if prefix.is_empty() {
            return Ok(None);
        }
        let exact: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sandbox_runs WHERE id = ?1",
                params![prefix],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)?;
        if exact {
            return Ok(Some(prefix.to_string()));
        }
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let mut stmt =
            conn.prepare("SELECT id FROM sandbox_runs WHERE id LIKE ?1 || '%' ESCAPE '\\'")?;
        let ids: Vec<String> = stmt
            .query_map(params![escaped], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        match ids.len() {
            0 => Ok(None),
            1 => Ok(Some(ids.into_iter().next().expect("one id"))),
            _ => Err(anyhow::anyhow!(
                "ambiguous sandbox id prefix '{prefix}': matches {} sandboxes ({})",
                ids.len(),
                ids.join(", ")
            )),
        }
    }
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::sandbox::Sandbox;
    use chrono::Utc;
    use std::path::PathBuf;

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    fn test_sandbox() -> Sandbox {
        Sandbox {
            id: "test-sandbox-id".to_string(),
            project_hash: "abcd1234".to_string(),
            base_branch: "main".to_string(),
            sandbox_branch: "canopy/sandbox-test".to_string(),
            worktree_path: PathBuf::from("/tmp/test-worktree"),
            cli_name: "opencode".to_string(),
            original_workdir: "/home/user/project".to_string(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn test_sandbox_run_roundtrip() {
        let db = test_db();
        let sandbox = test_sandbox();
        db.insert_sandbox_run(&sandbox, "loop", "loop-123").unwrap();

        let retrieved = db.get_sandbox_run("test-sandbox-id").unwrap().unwrap();
        assert_eq!(retrieved.id, "test-sandbox-id");
        assert_eq!(retrieved.project_hash, "abcd1234");
        assert_eq!(retrieved.base_branch, "main");
        assert_eq!(retrieved.sandbox_branch, "canopy/sandbox-test");
        assert_eq!(retrieved.cli_name, "opencode");
        assert_eq!(retrieved.owner_type, "loop");
        assert_eq!(retrieved.owner_id, "loop-123");
        assert_eq!(retrieved.status, "active");
    }

    #[test]
    fn test_sandbox_run_status_transitions() {
        let db = test_db();
        let sandbox = test_sandbox();
        db.insert_sandbox_run(&sandbox, "loop", "loop-123").unwrap();

        db.update_sandbox_run_status("test-sandbox-id", "merging")
            .unwrap();
        let r = db.get_sandbox_run("test-sandbox-id").unwrap().unwrap();
        assert_eq!(r.status, "merging");

        db.update_sandbox_run_status("test-sandbox-id", "merged")
            .unwrap();
        let r = db.get_sandbox_run("test-sandbox-id").unwrap().unwrap();
        assert_eq!(r.status, "merged");
    }

    #[test]
    fn test_abandoned_sandboxes_detected() {
        let db = test_db();
        let sandbox = test_sandbox();
        db.insert_sandbox_run(&sandbox, "loop", "loop-123").unwrap();

        let active = db.list_active_sandbox_runs().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "test-sandbox-id");

        db.update_sandbox_run_status("test-sandbox-id", "merged")
            .unwrap();
        let active = db.list_active_sandbox_runs().unwrap();
        assert_eq!(active.len(), 0);
    }

    #[test]
    fn test_cleanup_error_roundtrip_and_finished_list() {
        let db = test_db();
        let sandbox = test_sandbox();
        db.insert_sandbox_run(&sandbox, "loop", "loop-123").unwrap();

        // A fresh row carries no error.
        let row = db.get_sandbox_run("test-sandbox-id").unwrap().unwrap();
        assert_eq!(row.cleanup_error, None);

        db.set_sandbox_cleanup_error("test-sandbox-id", "/tmp/wt: failed: boom")
            .unwrap();
        let row = db.get_sandbox_run("test-sandbox-id").unwrap().unwrap();
        assert_eq!(row.cleanup_error, Some("/tmp/wt: failed: boom".to_string()));

        // `active` rows are not finished; terminal ones are.
        assert!(db.list_finished_sandbox_runs().unwrap().is_empty());
        assert_eq!(db.list_all_sandbox_runs().unwrap().len(), 1);
        db.update_sandbox_run_status("test-sandbox-id", "kept")
            .unwrap();
        let finished = db.list_finished_sandbox_runs().unwrap();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].id, "test-sandbox-id");
        assert_eq!(
            finished[0].cleanup_error,
            Some("/tmp/wt: failed: boom".to_string())
        );
    }
}
