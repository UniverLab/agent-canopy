#![allow(dead_code)]
//! SQLite repositories for projects and RAG queue.
//!
//! RAG chunks and embeddings are stored in LanceDB (`~/.canopy/rag/vectors.lancedb`).
//! Only the indexing queue (`rag_queue`) remains in SQLite for coordination.

use anyhow::Result;
use rusqlite::params;
use std::path::Path;

use crate::db::Database;
use crate::domain::project::{
    extract_readme_description, workdir_hash, Project, RemapCounts, RemapKind,
};

/// Outcome of a project remap (real or previewed): which case it was, the
/// hashes/path involved, and the per-table row counts touched.
#[derive(Debug, Clone)]
pub struct RemapOutcome {
    pub kind: RemapKind,
    pub old_hash: String,
    pub new_hash: String,
    pub new_path: String,
    pub counts: RemapCounts,
}

/// Resolve `path` to the canonical form a remap should key dependents on.
/// Refuses a path that doesn't exist on disk unless `force` is set, in which
/// case the path is absolutized (relative to cwd) but left otherwise
/// unverified — there's nothing on disk to canonicalize against.
pub fn resolve_remap_path(path: &Path, force: bool) -> Result<String> {
    match std::fs::canonicalize(path) {
        Ok(canonical) => Ok(canonical.to_string_lossy().to_string()),
        Err(_) if force => {
            let abs = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            };
            Ok(abs.to_string_lossy().to_string())
        }
        Err(e) => Err(anyhow::anyhow!(
            "New path {} does not exist ({e}); use --force to remap anyway",
            path.display()
        )),
    }
}

#[derive(Debug, Clone)]
pub struct RagQueueItem {
    pub source_path: String,
    pub status: String,
    pub queued_at: i64,
}

/// One row in a project's persisted History tab: a finished loop or a past
/// (no longer live) interactive/terminal session, scoped to the project's
/// workdir and ordered newest-first. Unlike the live agent/loop providers
/// (which only know about what's running in *this* TUI process), this reads
/// straight from SQLite so history survives a restart.
#[derive(Debug, Clone)]
pub struct ProjectHistoryEntry {
    pub kind: ProjectHistoryKind,
    pub name: String,
    pub status: String,
    /// Unix timestamp used for sorting and relative-time display.
    pub at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectHistoryKind {
    Loop,
    InteractiveSession,
    TerminalSession,
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
        {
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
        }

        // Keep the intelligence graph root in sync: every registered project
        // must exist as a kind='project' node or link_projects and the TUI
        // relation picker have nothing to operate on.
        self.ensure_project_node(p)?;
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

    pub fn unregister_project_path(&self, path: &Path) -> Result<()> {
        if let Ok(Some(project)) = self.get_project_by_path(path) {
            self.delete_project(&project.hash)?;
        }
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

    pub fn clear_rag_file_events(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute("DELETE FROM rag_file_events", [])?;
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

    pub fn get_project_by_path_or_ancestor(&self, path: &Path) -> Result<Option<Project>> {
        let canonical = std::fs::canonicalize(path)?;
        let projects = self.list_projects()?;

        Ok(projects
            .into_iter()
            .filter_map(|project| {
                let project_path = std::path::PathBuf::from(&project.path);
                canonical
                    .starts_with(&project_path)
                    .then_some((project_path.components().count(), project))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, project)| project))
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

    /// Preview what [`Database::remap_project`] would do, without changing
    /// anything: which rows would move and whether it's a MOVE or a MERGE.
    /// `new_canonical_path` must already be resolved (see
    /// [`resolve_remap_path`]) — this function does no filesystem I/O.
    pub fn remap_preview(&self, old_hash: &str, new_canonical_path: &str) -> Result<RemapOutcome> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let tx = conn.unchecked_transaction()?;

        let old_path: String = tx
            .query_row(
                "SELECT path FROM projects WHERE hash = ?1",
                params![old_hash],
                |row| row.get(0),
            )
            .map_err(|_| anyhow::anyhow!("No project registered with hash '{old_hash}'"))?;

        let new_hash = workdir_hash(new_canonical_path);
        if new_hash == old_hash {
            anyhow::bail!(
                "New path resolves to the same project (hash unchanged) — nothing to remap"
            );
        }

        let target_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE hash = ?1)",
            params![new_hash],
            |row| row.get::<_, i64>(0),
        )? != 0;
        let counts = count_remap_targets(&tx, &old_path, old_hash)?;

        Ok(RemapOutcome {
            kind: RemapKind::decide(target_exists),
            old_hash: old_hash.to_string(),
            new_hash,
            new_path: new_canonical_path.to_string(),
            counts,
        })
    }

    /// Remap a project registered at `old_hash` onto `new_canonical_path`:
    /// re-key every dependent row (interactive/terminal sessions, loops,
    /// standalone specs, sync state, prompts, scheduled sends, agents,
    /// intelligence nodes) from the old path/hash to the new one, in a
    /// single transaction. If no project is registered at the new path
    /// (MOVE), the project row itself is updated in place; if one already
    /// exists (MERGE), dependents are folded into it and the stale row is
    /// removed. `new_canonical_path` must already be resolved (see
    /// [`resolve_remap_path`]) — this function does no filesystem I/O.
    ///
    /// A crash mid-transaction leaves every dependent attached to exactly
    /// one project: either the whole re-key applied (and committed) or none
    /// of it did.
    pub fn remap_project(&self, old_hash: &str, new_canonical_path: &str) -> Result<RemapOutcome> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let tx = conn.transaction()?;

        let old_path: String = tx
            .query_row(
                "SELECT path FROM projects WHERE hash = ?1",
                params![old_hash],
                |row| row.get(0),
            )
            .map_err(|_| anyhow::anyhow!("No project registered with hash '{old_hash}'"))?;

        let new_hash = workdir_hash(new_canonical_path);
        if new_hash == old_hash {
            anyhow::bail!(
                "New path resolves to the same project (hash unchanged) — nothing to remap"
            );
        }

        let target_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE hash = ?1)",
            params![new_hash],
            |row| row.get::<_, i64>(0),
        )? != 0;
        let kind = RemapKind::decide(target_exists);
        let counts = count_remap_targets(&tx, &old_path, old_hash)?;

        tx.execute(
            "UPDATE interactive_sessions SET working_dir = ?2 WHERE working_dir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE terminal_sessions SET working_dir = ?2 WHERE working_dir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE loops SET workdir = ?2 WHERE workdir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE loop_specs SET workdir = ?2 WHERE workdir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE sync_messages SET workdir = ?2 WHERE workdir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE sync_locks SET workdir = ?2 WHERE workdir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE last_prompts SET workdir = ?2 WHERE workdir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE scheduled_sends SET workdir = ?2 WHERE workdir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE failed_scheduled_sends SET workdir = ?2 WHERE workdir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE agents SET working_dir = ?2 WHERE working_dir = ?1",
            params![old_path, new_canonical_path],
        )?;
        tx.execute(
            "UPDATE intelligence_nodes SET project_hash = ?2 WHERE project_hash = ?1",
            params![old_hash, new_hash],
        )?;

        match kind {
            RemapKind::Move => {
                tx.execute(
                    "UPDATE projects SET hash = ?2, path = ?3 WHERE hash = ?1",
                    params![old_hash, new_hash, new_canonical_path],
                )?;
            }
            RemapKind::Merge => {
                tx.execute("DELETE FROM projects WHERE hash = ?1", params![old_hash])?;
            }
        }

        tx.commit()?;

        Ok(RemapOutcome {
            kind,
            old_hash: old_hash.to_string(),
            new_hash,
            new_path: new_canonical_path.to_string(),
            counts,
        })
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

    /// Recover queue items that were left in `processing` after an unexpected
    /// daemon exit. Moves them back to `queued` so indexing can resume.
    pub fn requeue_processing_rag_items(&self, now: i64) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let n = conn.execute(
            "UPDATE rag_queue
             SET status='queued', queued_at=?1, updated_at=?1
             WHERE status='processing'",
            rusqlite::params![now],
        )?;
        Ok(n)
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

/// Shared counting logic for [`Database::remap_preview`] (read-only) and
/// [`Database::remap_project`] (counted just before the re-key runs, inside
/// the same transaction) — both call sites must see identical numbers, so
/// this is the only place the counting SQL lives. Every project-scoped table
/// found by grepping for a `workdir`/`working_dir`/`project_hash` column
/// (see `src/db/mod.rs`'s schema) is covered here.
fn count_remap_targets(
    tx: &rusqlite::Transaction,
    old_path: &str,
    old_hash: &str,
) -> Result<RemapCounts> {
    let interactive_sessions: i64 = tx.query_row(
        "SELECT COUNT(*) FROM interactive_sessions WHERE working_dir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let terminal_sessions: i64 = tx.query_row(
        "SELECT COUNT(*) FROM terminal_sessions WHERE working_dir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let loops: i64 = tx.query_row(
        "SELECT COUNT(*) FROM loops WHERE workdir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let loop_specs: i64 = tx.query_row(
        "SELECT COUNT(*) FROM loop_specs WHERE workdir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let sync_messages: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sync_messages WHERE workdir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let sync_locks: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sync_locks WHERE workdir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let last_prompts: i64 = tx.query_row(
        "SELECT COUNT(*) FROM last_prompts WHERE workdir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let scheduled_sends: i64 = tx.query_row(
        "SELECT COUNT(*) FROM scheduled_sends WHERE workdir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let failed_scheduled_sends: i64 = tx.query_row(
        "SELECT COUNT(*) FROM failed_scheduled_sends WHERE workdir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let agents: i64 = tx.query_row(
        "SELECT COUNT(*) FROM agents WHERE working_dir = ?1",
        params![old_path],
        |row| row.get(0),
    )?;
    let intelligence_nodes: i64 = tx.query_row(
        "SELECT COUNT(*) FROM intelligence_nodes WHERE project_hash = ?1",
        params![old_hash],
        |row| row.get(0),
    )?;

    Ok(RemapCounts {
        interactive_sessions,
        terminal_sessions,
        loops,
        loop_specs,
        sync_messages,
        sync_locks,
        last_prompts,
        scheduled_sends,
        failed_scheduled_sends,
        agents,
        intelligence_nodes,
    })
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

// ── rag_file_events ────────────────────────────────────────────────────────

/// A recorded lifecycle event for an indexed file.
#[derive(Debug, Clone)]
pub struct RagFileEvent {
    pub id: i64,
    pub file_path: String,
    /// `"indexed"` | `"deleted"` | `"error"` | `"failed"` (permanent give-up)
    /// | `"skipped_oversize"` (exceeds `FILE_MAX_BYTES`, `detail` holds size in bytes)
    pub event_type: String,
    pub detail: Option<String>,
    pub occurred_at: i64,
}

impl Database {
    /// Append a lifecycle event for a RAG file.
    pub fn log_rag_event(
        &self,
        file_path: &str,
        event_type: &str,
        detail: Option<&str>,
        occurred_at: i64,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO rag_file_events (file_path, event_type, detail, occurred_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![file_path, event_type, detail, occurred_at],
        )?;
        Ok(())
    }

    /// Return all events for a specific file, newest-first.
    pub fn rag_events_for_file(&self, file_path: &str) -> Result<Vec<RagFileEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, file_path, event_type, detail, occurred_at
               FROM rag_file_events
              WHERE file_path = ?1
              ORDER BY occurred_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![file_path], row_to_rag_file_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Return all events, newest-first, optionally limited.
    pub fn list_rag_events(&self, limit: usize) -> Result<Vec<RagFileEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, file_path, event_type, detail, occurred_at
               FROM rag_file_events
              ORDER BY occurred_at DESC, id DESC
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], row_to_rag_file_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Return the most-recent error event per file path.
    pub fn rag_last_error_per_file(&self) -> Result<Vec<RagFileEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, file_path, event_type, detail, occurred_at
               FROM rag_file_events
              WHERE event_type = 'error'
                AND occurred_at = (
                    SELECT MAX(occurred_at) FROM rag_file_events e2
                     WHERE e2.file_path = rag_file_events.file_path
                       AND e2.event_type = 'error'
                )
              ORDER BY file_path",
        )?;
        let rows = stmt.query_map([], row_to_rag_file_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Return a map of file_path → last_indexed_at (unix seconds) for all files
    /// that have at least one successful `"indexed"` event and were not later
    /// deleted. Error events do not invalidate the last successful timestamp.
    /// Used at startup to skip re-indexing files that haven't changed since
    /// they were last indexed successfully.
    pub fn indexed_files_timestamps(&self) -> Result<std::collections::HashMap<String, i64>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        // Keep the last successful index timestamp per file unless a later delete
        // exists. This avoids full startup re-index loops after transient errors.
        let mut stmt = conn.prepare(
            "SELECT s.file_path, s.last_indexed_at
               FROM (
                   SELECT
                       file_path,
                       MAX(CASE WHEN event_type = 'indexed' THEN occurred_at END) AS last_indexed_at,
                       MAX(CASE WHEN event_type = 'deleted' THEN occurred_at END) AS last_deleted_at
                   FROM rag_file_events
                   GROUP BY file_path
               ) s
              WHERE s.last_indexed_at IS NOT NULL
                AND (s.last_deleted_at IS NULL OR s.last_indexed_at > s.last_deleted_at)",
        )?;
        let mut map = std::collections::HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let ts: i64 = row.get(1)?;
            map.insert(path, ts);
        }
        Ok(map)
    }

    /// Count of `"error"` events recorded for a given file path.
    pub fn rag_error_count(&self, file_path: &str) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rag_file_events WHERE file_path = ?1 AND event_type = 'error'",
            rusqlite::params![file_path],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Return the set of file paths that have been permanently given up on:
    /// at least one `"failed"` event with no later `"indexed"` event. A
    /// successful re-index (e.g. after a manual re-add) clears the file from
    /// this set, mirroring how `indexed_files_timestamps` treats `"deleted"`.
    pub fn permanently_failed_rag_files(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT file_path
               FROM (
                   SELECT
                       file_path,
                       MAX(CASE WHEN event_type = 'failed' THEN occurred_at END) AS last_failed_at,
                       MAX(CASE WHEN event_type = 'indexed' THEN occurred_at END) AS last_indexed_at
                   FROM rag_file_events
                   GROUP BY file_path
               ) s
              WHERE s.last_failed_at IS NOT NULL
                AND (s.last_indexed_at IS NULL OR s.last_failed_at > s.last_indexed_at)",
        )?;
        let mut set = std::collections::HashSet::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            set.insert(path);
        }
        Ok(set)
    }
}

fn row_to_rag_file_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<RagFileEvent> {
    Ok(RagFileEvent {
        id: row.get(0)?,
        file_path: row.get(1)?,
        event_type: row.get(2)?,
        detail: row.get(3)?,
        occurred_at: row.get(4)?,
    })
}

// ── RagPerFileStatus ────────────────────────────────────────────────────────

/// Aggregated per-file RAG status for the TUI preview panel.
#[derive(Debug, Clone)]
pub struct RagPerFileStatus {
    pub file_path: String,
    /// Last recorded event type: `"indexed"` | `"deleted"` | `"error"`
    pub last_event_type: String,
    /// Detail from the last event (error message, etc.)
    pub last_detail: Option<String>,
    /// How many times this file has been successfully indexed.
    pub times_indexed: i64,
    /// Timestamp of the most recent event.
    pub last_at: i64,
}

impl Database {
    /// Return one summary row per file: last event, last detail, index count.
    /// Results are ordered by last activity time, newest first.
    pub fn rag_per_file_status(&self) -> Result<Vec<RagPerFileStatus>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT
                 e.file_path,
                 e.event_type,
                 e.detail,
                 COALESCE(ic.cnt, 0),
                 e.occurred_at
             FROM rag_file_events e
             INNER JOIN (
                 SELECT file_path, MAX(occurred_at) AS max_at
                   FROM rag_file_events
                  GROUP BY file_path
             ) latest ON e.file_path = latest.file_path
                     AND e.occurred_at = latest.max_at
             LEFT JOIN (
                 SELECT file_path, COUNT(*) AS cnt
                   FROM rag_file_events
                  WHERE event_type = 'indexed'
                  GROUP BY file_path
             ) ic ON e.file_path = ic.file_path
             ORDER BY e.occurred_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RagPerFileStatus {
                file_path: row.get(0)?,
                last_event_type: row.get(1)?,
                last_detail: row.get(2)?,
                times_indexed: row.get(3)?,
                last_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

fn parse_rfc3339_timestamp(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

impl Database {
    /// Persisted history for a project's `Focus → History` tab: finished
    /// loops plus past (exited/finished) interactive and terminal sessions,
    /// all scoped to `workdir` and merged newest-first. Reads straight from
    /// SQLite (via the `idx_loops_workdir_created`,
    /// `idx_interactive_sessions_workdir`, and `idx_terminal_sessions_workdir`
    /// indices) rather than the live agent/loop providers, which only know
    /// about state observed since this TUI process started.
    pub fn list_project_history(
        &self,
        workdir: &str,
        limit: usize,
    ) -> Result<Vec<ProjectHistoryEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        let mut entries = Vec::new();

        let mut loop_stmt = conn.prepare(
            "SELECT name, status, COALESCE(completed_at, created_at)
             FROM loops
             WHERE workdir = ?1 AND status IN ('completed', 'failed')
             ORDER BY COALESCE(completed_at, created_at) DESC
             LIMIT ?2",
        )?;
        let loop_rows = loop_stmt.query_map(rusqlite::params![workdir, limit as i64], |row| {
            Ok(ProjectHistoryEntry {
                kind: ProjectHistoryKind::Loop,
                name: row.get(0)?,
                status: row.get(1)?,
                at: row.get(2)?,
            })
        })?;
        for row in loop_rows {
            entries.push(row?);
        }

        let mut interactive_stmt = conn.prepare(
            "SELECT name, status, COALESCE(exited_at, started_at)
             FROM interactive_sessions
             WHERE working_dir = ?1 AND status != 'active'
             ORDER BY COALESCE(exited_at, started_at) DESC
             LIMIT ?2",
        )?;
        let interactive_rows =
            interactive_stmt.query_map(rusqlite::params![workdir, limit as i64], |row| {
                let name: String = row.get(0)?;
                let status: String = row.get(1)?;
                let at: String = row.get(2)?;
                Ok((name, status, at))
            })?;
        for row in interactive_rows {
            let (name, status, at) = row?;
            entries.push(ProjectHistoryEntry {
                kind: ProjectHistoryKind::InteractiveSession,
                name,
                status,
                at: parse_rfc3339_timestamp(&at),
            });
        }

        let mut terminal_stmt = conn.prepare(
            "SELECT name, status, COALESCE(last_active, created_at)
             FROM terminal_sessions
             WHERE working_dir = ?1 AND status != 'idle'
             ORDER BY COALESCE(last_active, created_at) DESC
             LIMIT ?2",
        )?;
        let terminal_rows =
            terminal_stmt.query_map(rusqlite::params![workdir, limit as i64], |row| {
                let name: String = row.get(0)?;
                let status: String = row.get(1)?;
                let at: String = row.get(2)?;
                Ok((name, status, at))
            })?;
        for row in terminal_rows {
            let (name, status, at) = row?;
            entries.push(ProjectHistoryEntry {
                kind: ProjectHistoryKind::TerminalSession,
                name,
                status,
                at: parse_rfc3339_timestamp(&at),
            });
        }

        entries.sort_by_key(|b| std::cmp::Reverse(b.at));
        entries.truncate(limit);
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::AgentRepository;

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    fn sample_project(hash: &str) -> Project {
        Project {
            hash: hash.to_string(),
            path: format!("/tmp/{hash}"),
            name: format!("Project {hash}"),
            description: Some("Test project".to_string()),
            tags: None,
            indexed_at: None,
            created_at: chrono::Utc::now().timestamp(),
        }
    }

    #[test]
    fn upsert_and_get_project() {
        let db = test_db();
        let project = sample_project("abc123");
        db.upsert_project(&project).unwrap();

        let retrieved = db.get_project("abc123").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.hash, "abc123");
        assert_eq!(retrieved.name, "Project abc123");
    }

    #[test]
    fn get_project_not_found() {
        let db = test_db();
        let result = db.get_project("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_projects_empty() {
        let db = test_db();
        let projects = db.list_projects().unwrap();
        assert!(projects.is_empty());
    }

    #[test]
    fn list_projects_with_projects() {
        let db = test_db();
        let project1 = sample_project("proj1");
        let project2 = sample_project("proj2");
        db.upsert_project(&project1).unwrap();
        db.upsert_project(&project2).unwrap();

        let projects = db.list_projects().unwrap();
        assert_eq!(projects.len(), 2);
    }

    #[test]
    fn delete_project() {
        let db = test_db();
        let project = sample_project("test-proj");
        db.upsert_project(&project).unwrap();

        db.delete_project("test-proj").unwrap();
        let retrieved = db.get_project("test-proj").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn update_project_description() {
        let db = test_db();
        let project = sample_project("test-proj");
        db.upsert_project(&project).unwrap();

        db.update_project_meta("test-proj", Some("New description"), None)
            .unwrap();
        let retrieved = db.get_project("test-proj").unwrap().unwrap();
        assert_eq!(retrieved.description, Some("New description".to_string()));
    }

    #[test]
    fn search_projects_by_name() {
        let db = test_db();
        let project1 = Project {
            hash: "proj1".to_string(),
            path: "/tmp/proj1".to_string(),
            name: "Rust Project".to_string(),
            description: Some("A Rust project".to_string()),
            tags: None,
            indexed_at: None,
            created_at: chrono::Utc::now().timestamp(),
        };
        let project2 = Project {
            hash: "proj2".to_string(),
            path: "/tmp/proj2".to_string(),
            name: "Python Project".to_string(),
            description: Some("A Python project".to_string()),
            tags: None,
            indexed_at: None,
            created_at: chrono::Utc::now().timestamp(),
        };
        db.upsert_project(&project1).unwrap();
        db.upsert_project(&project2).unwrap();

        let results = db.search_projects("Rust").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hash, "proj1");
    }

    #[test]
    fn search_projects_by_description() {
        let db = test_db();
        let project = Project {
            hash: "proj1".to_string(),
            path: "/tmp/proj1".to_string(),
            name: "My Project".to_string(),
            description: Some("Contains Rust code".to_string()),
            tags: None,
            indexed_at: None,
            created_at: chrono::Utc::now().timestamp(),
        };
        db.upsert_project(&project).unwrap();

        let results = db.search_projects("Rust").unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn parse_rfc3339_timestamp_valid() {
        let ts = parse_rfc3339_timestamp("2024-01-15T10:30:00Z");
        assert!(ts > 0);
    }

    #[test]
    fn parse_rfc3339_timestamp_invalid() {
        let ts = parse_rfc3339_timestamp("invalid");
        assert_eq!(ts, 0);
    }

    // ── resolve_remap_path ──────────────────────────────────────────────

    #[test]
    fn resolve_remap_path_canonicalizes_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_remap_path(dir.path(), false).unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path()).unwrap().to_string_lossy()
        );
    }

    #[test]
    fn resolve_remap_path_refuses_missing_dir_without_force() {
        let err = resolve_remap_path(Path::new("/definitely/does/not/exist"), false).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn resolve_remap_path_allows_missing_dir_with_force() {
        let resolved = resolve_remap_path(Path::new("/definitely/does/not/exist"), true).unwrap();
        assert_eq!(resolved, "/definitely/does/not/exist");
    }

    // ── remap_project / remap_preview ───────────────────────────────────

    fn seed_full_dependents(db: &Database, hash: &str, workdir: &str) {
        db.insert_interactive_session(
            "sess-1",
            "sess-1",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.insert_terminal_session("term-1", "term-1", "bash", workdir)
            .unwrap();
        db.insert_loop(&crate::domain::loops::Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
            id: "loop-1".to_string(),
            name: "loop-1".to_string(),
            description: None,
            workdir: workdir.to_string(),
            status: crate::domain::loops::LoopStatus::Completed,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        })
        .unwrap();
        db.insert_loop_spec(&crate::domain::loops::LoopSpec {
            id: "spec-standalone-1".to_string(),
            loop_id: None,
            name: "standalone".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: crate::domain::loops::LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: Some(workdir.to_string()),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        db.insert_sync_message(
            workdir,
            "agent-1",
            "Agent One",
            crate::domain::sync::MessageKind::Info,
            "hello",
            None,
        )
        .unwrap();
        db.insert_sync_lock_for_test("lock-1", workdir).unwrap();
        db.insert_last_prompt(
            "prompt-1",
            workdir,
            "do the thing",
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        db.insert_scheduled_send(
            "send-1",
            "ping",
            "sess-1",
            Some(workdir),
            chrono::Utc::now(),
            None,
        )
        .unwrap();
        db.insert_failed_scheduled_send(
            "failed-1",
            "ping",
            "sess-1",
            Some(workdir),
            chrono::Utc::now(),
        )
        .unwrap();
        db.upsert_agent(&crate::domain::models::Agent {
            id: "agent-bg-1".to_string(),
            prompt: "do stuff".to_string(),
            trigger: None,
            cli: crate::domain::models::Cli::new("opencode"),
            model: None,
            working_dir: Some(workdir.to_string()),
            enabled: true,
            enable_at: None,
            created_at: chrono::Utc::now(),
            log_path: "/tmp/agent-bg-1.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        })
        .unwrap();
        db.upsert_intelligence_node(crate::db::intelligence::IntelligenceNodeInput {
            id: None,
            kind: "fact".to_string(),
            title: "a fact".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: Some(hash.to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();
    }

    #[test]
    fn remap_project_move_rekeys_every_dependent_table() {
        let db = test_db();
        let old_hash = "hash-old";
        let old_path = "/proj/old-location";
        let new_path = "/proj/new-location";

        db.upsert_project(&sample_project_at(old_hash, old_path))
            .unwrap();
        seed_full_dependents(&db, old_hash, old_path);

        let outcome = db.remap_project(old_hash, new_path).unwrap();

        assert_eq!(outcome.kind, RemapKind::Move);
        assert_eq!(outcome.old_hash, old_hash);
        assert_eq!(outcome.new_hash, workdir_hash(new_path));
        assert_eq!(outcome.new_path, new_path);
        // Every seeded table contributed exactly one dependent row.
        assert_eq!(outcome.counts.interactive_sessions, 1);
        assert_eq!(outcome.counts.terminal_sessions, 1);
        assert_eq!(outcome.counts.loops, 1);
        assert_eq!(outcome.counts.loop_specs, 1);
        assert_eq!(outcome.counts.sync_messages, 1);
        assert_eq!(outcome.counts.sync_locks, 1);
        assert_eq!(outcome.counts.last_prompts, 1);
        assert_eq!(outcome.counts.scheduled_sends, 1);
        assert_eq!(outcome.counts.failed_scheduled_sends, 1);
        assert_eq!(outcome.counts.agents, 1);
        // The project-root node `upsert_project` auto-creates, plus the fact
        // node `seed_full_dependents` inserts.
        assert_eq!(outcome.counts.intelligence_nodes, 2);
        assert_eq!(outcome.counts.total(), 12);

        // The project kept its identity but moved hash/path.
        assert!(db.get_project(old_hash).unwrap().is_none());
        let moved = db.get_project(&outcome.new_hash).unwrap().unwrap();
        assert_eq!(moved.path, new_path);

        // Every dependent now points at the new path/hash, and nothing is
        // left behind at the old one.
        assert_eq!(db.project_dependent_counts(old_path).unwrap().loops, 0);
        assert_eq!(db.project_dependent_counts(new_path).unwrap().loops, 1);
        assert_eq!(
            db.project_dependent_counts(new_path)
                .unwrap()
                .interactive_sessions,
            1
        );
        assert_eq!(
            db.project_dependent_counts(new_path)
                .unwrap()
                .terminal_sessions,
            1
        );
        let agent = db.get_agent("agent-bg-1").unwrap().unwrap();
        assert_eq!(agent.working_dir.as_deref(), Some(new_path));
    }

    #[test]
    fn remap_project_merge_reassigns_dependents_and_removes_stale_row() {
        let db = test_db();
        let old_hash = "hash-cadforge";
        let old_path = "/proj/cadforge";
        let new_path = "/proj/cadspec";
        let new_hash = workdir_hash(new_path);

        db.upsert_project(&sample_project_at(old_hash, old_path))
            .unwrap();
        seed_full_dependents(&db, old_hash, old_path);
        // The rename target is already a registered project (the C2 orphan
        // scenario from the spec: cadforge renamed to cadspec, and cadspec
        // was separately re-registered under its own hash).
        db.upsert_project(&sample_project_at(&new_hash, new_path))
            .unwrap();

        let outcome = db.remap_project(old_hash, new_path).unwrap();

        assert_eq!(outcome.kind, RemapKind::Merge);
        assert_eq!(outcome.new_hash, new_hash);
        assert_eq!(outcome.counts.total(), 12);

        // Exactly one project row survives, at the new hash.
        assert!(db.get_project(old_hash).unwrap().is_none());
        assert!(db.get_project(&new_hash).unwrap().is_some());
        assert_eq!(
            db.list_projects()
                .unwrap()
                .iter()
                .filter(|p| p.path == new_path || p.path == old_path)
                .count(),
            1
        );

        // Dependents were reassigned to the new path/hash.
        assert_eq!(db.project_dependent_counts(new_path).unwrap().loops, 1);
        assert_eq!(db.project_dependent_counts(old_path).unwrap().loops, 0);
    }

    #[test]
    fn remap_project_unrelated_project_left_intact() {
        let db = test_db();
        db.upsert_project(&sample_project_at("hash-a", "/proj-a"))
            .unwrap();
        db.upsert_project(&sample_project_at("hash-b", "/proj-b"))
            .unwrap();
        db.insert_loop(&crate::domain::loops::Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
            id: "loop-b".to_string(),
            name: "loop-b".to_string(),
            description: None,
            workdir: "/proj-b".to_string(),
            status: crate::domain::loops::LoopStatus::Completed,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        })
        .unwrap();

        db.remap_project("hash-a", "/proj-a-moved").unwrap();

        assert!(db.get_project("hash-b").unwrap().is_some());
        assert_eq!(db.project_dependent_counts("/proj-b").unwrap().loops, 1);
    }

    #[test]
    fn remap_project_errors_when_hash_not_registered() {
        let db = test_db();
        let err = db.remap_project("no-such-hash", "/proj/new").unwrap_err();
        assert!(err.to_string().contains("no-such-hash"));
    }

    #[test]
    fn remap_project_refuses_when_new_path_resolves_to_same_project() {
        let db = test_db();
        let path = "/proj/unchanged";
        let hash = workdir_hash(path);
        db.upsert_project(&sample_project_at(&hash, path)).unwrap();

        let err = db.remap_project(&hash, path).unwrap_err();
        assert!(err.to_string().contains("same project"));
    }

    #[test]
    fn remap_preview_reports_counts_without_changing_anything() {
        let db = test_db();
        let old_hash = "hash-preview";
        let old_path = "/proj/preview-old";
        let new_path = "/proj/preview-new";

        db.upsert_project(&sample_project_at(old_hash, old_path))
            .unwrap();
        seed_full_dependents(&db, old_hash, old_path);

        let preview = db.remap_preview(old_hash, new_path).unwrap();
        assert_eq!(preview.kind, RemapKind::Move);
        assert_eq!(preview.counts.total(), 12);

        // Nothing actually moved.
        assert!(db.get_project(old_hash).unwrap().is_some());
        assert!(db.get_project(&workdir_hash(new_path)).unwrap().is_none());
        assert_eq!(db.project_dependent_counts(old_path).unwrap().loops, 1);
        assert_eq!(db.project_dependent_counts(new_path).unwrap().loops, 0);

        // A real remap right after reports the same counts.
        let applied = db.remap_project(old_hash, new_path).unwrap();
        assert_eq!(applied.counts, preview.counts);
    }

    #[test]
    fn remap_preview_merge_kind_when_target_already_registered() {
        let db = test_db();
        let old_hash = "hash-preview-merge";
        let old_path = "/proj/preview-merge-old";
        let new_path = "/proj/preview-merge-new";
        let new_hash = workdir_hash(new_path);

        db.upsert_project(&sample_project_at(old_hash, old_path))
            .unwrap();
        db.upsert_project(&sample_project_at(&new_hash, new_path))
            .unwrap();

        let preview = db.remap_preview(old_hash, new_path).unwrap();
        assert_eq!(preview.kind, RemapKind::Merge);

        // Still just previewing: both project rows remain.
        assert!(db.get_project(old_hash).unwrap().is_some());
        assert!(db.get_project(&new_hash).unwrap().is_some());
    }

    fn sample_project_at(hash: &str, path: &str) -> Project {
        Project {
            hash: hash.to_string(),
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            description: None,
            tags: None,
            indexed_at: None,
            created_at: chrono::Utc::now().timestamp(),
        }
    }
}
