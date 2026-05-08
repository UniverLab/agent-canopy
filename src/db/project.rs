#![allow(dead_code)]
//! SQLite repositories for projects and RAG queue.
//!
//! RAG chunks and embeddings are stored in LanceDB (`~/.canopy/rag/vectors.lancedb`).
//! Only the indexing queue (`rag_queue`) remains in SQLite for coordination.

use anyhow::Result;
use std::path::Path;

use crate::db::Database;
use crate::domain::project::{extract_readme_description, Project};

#[derive(Debug, Clone)]
pub struct RagQueueItem {
    pub source_path: String,
    pub status: String,
    pub queued_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct RagInfoSummary {
    pub total_chunks: i64,
    pub indexed_files: i64,
    pub queued_items: i64,
    pub processing_items: i64,
}

impl RagInfoSummary {
    pub fn has_rag_activity(&self) -> bool {
        self.total_chunks > 0 || self.queued_items > 0 || self.processing_items > 0
    }
}

impl Database {
    // ── projects ───────────────────────────────────────────────────────

    pub fn upsert_project(&self, p: &Project) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO projects (hash, path, name, description, tags, indexed_at, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(hash) DO UPDATE SET
               path=excluded.path,
               name=excluded.name,
               description=COALESCE(description, excluded.description),
               tags=COALESCE(tags, excluded.tags),
               indexed_at=COALESCE(projects.indexed_at, excluded.indexed_at)",
            rusqlite::params![
                p.hash,
                p.path,
                p.name,
                p.description,
                p.tags,
                p.indexed_at,
                p.created_at
            ],
        )?;
        Ok(())
    }

    pub fn register_project_path(&self, path: &Path) -> Result<Project> {
        let canonical = std::fs::canonicalize(path)?;
        let canonical_str = canonical.to_string_lossy().to_string();
        let mut project = Project::new(&canonical_str);

        let readme_path = canonical.join("README.md");
        if readme_path.exists() {
            let readme = std::fs::read_to_string(&readme_path)?;
            project.description = extract_readme_description(&readme);
        }

        self.upsert_project(&project)?;
        Ok(project)
    }

    pub fn delete_project(&self, hash: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "DELETE FROM projects WHERE hash=?1",
            rusqlite::params![hash],
        )?;
        Ok(())
    }

    pub fn clear_rag_queue(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute("DELETE FROM rag_queue", [])?;
        Ok(())
    }

    pub fn get_project(&self, hash: &str) -> Result<Option<Project>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT hash,path,name,description,tags,indexed_at,created_at
             FROM projects WHERE hash=?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![hash], row_to_project)?;
        Ok(rows.next().transpose()?)
    }

    pub fn get_project_by_path(&self, path: &Path) -> Result<Option<Project>> {
        let canonical = std::fs::canonicalize(path)?;
        let canonical_str = canonical.to_string_lossy().to_string();
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT hash,path,name,description,tags,indexed_at,created_at
             FROM projects WHERE path=?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![canonical_str], row_to_project)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT hash,path,name,description,tags,indexed_at,created_at
             FROM projects ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_project)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn search_projects(&self, query: &str) -> Result<Vec<Project>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let pattern = format!("%{}%", query.to_lowercase());
        let mut stmt = conn.prepare(
            "SELECT hash,path,name,description,tags,indexed_at,created_at FROM projects
             WHERE lower(name) LIKE ?1 OR lower(description) LIKE ?1
             ORDER BY created_at DESC LIMIT 20",
        )?;
        let rows = stmt.query_map(rusqlite::params![pattern], row_to_project)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn update_project_meta(
        &self,
        hash: &str,
        description: Option<&str>,
        tags: Option<&[String]>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let tags_str = tags.map(|t| t.join(","));
        let n = conn.execute(
            "UPDATE projects SET
               description = COALESCE(?2, description),
               tags = COALESCE(?3, tags)
             WHERE hash = ?1",
            rusqlite::params![hash, description, tags_str],
        )?;
        Ok(n > 0)
    }

    pub fn mark_project_indexed(&self, hash: &str, indexed_at: i64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let n = conn.execute(
            "UPDATE projects SET indexed_at = ?2 WHERE hash = ?1",
            rusqlite::params![hash, indexed_at],
        )?;
        Ok(n > 0)
    }

    // ── RAG queue (SQLite) ──────────────────────────────────────────────

    pub fn enqueue_rag_item(&self, source_path: &str, queued_at: i64) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO rag_queue (source_path, status, queued_at, updated_at)
             VALUES (?1, 'queued', ?2, ?2)
             ON CONFLICT(source_path) DO UPDATE SET
               status='queued',
               queued_at=excluded.queued_at,
               updated_at=excluded.updated_at",
            rusqlite::params![source_path, queued_at],
        )?;
        Ok(())
    }

    pub fn mark_rag_item_processing(&self, source_path: &str, updated_at: i64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let n = conn.execute(
            "UPDATE rag_queue
             SET status='processing', updated_at=?2
             WHERE source_path=?1",
            rusqlite::params![source_path, updated_at],
        )?;
        Ok(n > 0)
    }

    pub fn remove_rag_item(&self, source_path: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let n = conn.execute(
            "DELETE FROM rag_queue WHERE source_path=?1",
            rusqlite::params![source_path],
        )?;
        Ok(n > 0)
    }

    pub fn list_rag_queue(&self, limit: usize) -> Result<Vec<RagQueueItem>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT source_path, status, queued_at
             FROM rag_queue
             ORDER BY CASE status WHEN 'processing' THEN 0 ELSE 1 END, queued_at ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], row_to_rag_queue_item)?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn rag_queue_counts(&self) -> Result<(i64, i64)> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let queued: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rag_queue WHERE status='queued'",
            [],
            |row| row.get(0),
        )?;
        let processing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rag_queue WHERE status='processing'",
            [],
            |row| row.get(0),
        )?;
        Ok((queued, processing))
    }
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        hash: row.get(0)?,
        path: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        tags: row.get(4)?,
        indexed_at: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn row_to_rag_queue_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<RagQueueItem> {
    Ok(RagQueueItem {
        source_path: row.get(0)?,
        status: row.get(1)?,
        queued_at: row.get(2)?,
    })
}
