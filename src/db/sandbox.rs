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
        let mut stmt = conn.prepare(
            "SELECT id, project_hash, base_branch, sandbox_branch, worktree_path, cli_name, original_workdir, owner_type, owner_id, created_at, status
             FROM sandbox_runs WHERE id = ?1",
        )?;
        let result = stmt
            .query_row(params![id], |row| {
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
                })
            })
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
        let mut stmt = conn.prepare(
            "SELECT id, project_hash, base_branch, sandbox_branch, worktree_path, cli_name, original_workdir, owner_type, owner_id, created_at, status
             FROM sandbox_runs WHERE status = 'active' ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
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
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
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
}
