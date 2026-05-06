#![allow(dead_code)]
//! SQLite repositories for projects and RAG chunks (FTS5).

use anyhow::Result;
use rusqlite::types::Type;
use std::path::Path;

use crate::db::Database;
use crate::domain::project::{extract_readme_description, Project};

// ── Chunk record (stored in FTS5 + metadata table) ─────────────────────

#[derive(Debug, Clone)]
pub struct Chunk {
    pub id: String,
    pub project_hash: Option<String>,
    pub source_path: String,
    pub chunk_index: i32,
    pub content: String,
    pub lang: String,
    pub embedding: Option<Vec<f32>>,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct ScoredChunk {
    pub chunk: Chunk,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct RagQueueItem {
    pub source_path: String,
    pub status: String,
    pub queued_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct RagInfoSummary {
    pub total_chunks: i64,
    pub indexed_projects: i64,
    pub queued_items: i64,
    pub processing_items: i64,
}

impl RagInfoSummary {
    /// True when there is any RAG activity — indexed chunks, pending queue, or
    /// active processing. Used to decide whether to show RAG UI panels.
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
        // With global RAG, deleting a project does NOT delete chunks.
        // Chunks are keyed by source_path, not project_hash.
        conn.execute(
            "DELETE FROM projects WHERE hash=?1",
            rusqlite::params![hash],
        )?;
        Ok(())
    }

    /// Remove all entries from the RAG queue (used on daemon startup to purge
    /// stale entries queued without the language filter in older versions).
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

    /// Full-text search over name + description.
    pub fn search_projects(&self, query: &str) -> Result<Vec<Project>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        // Simple LIKE search — FTS5 is on chunks, not projects (small table)
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
               tags        = COALESCE(?3, tags)
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

    /// Enqueue a file for RAG indexing. project_hash is optional metadata.
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

    pub fn rag_info_summary(&self) -> Result<RagInfoSummary> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        let total_chunks =
            conn.query_row("SELECT COUNT(*) FROM rag_chunks", [], |row| row.get(0))?;
        let indexed_files = conn.query_row(
            "SELECT COUNT(DISTINCT source_path) FROM rag_chunks",
            [],
            |row| row.get(0),
        )?;
        let queued_items = conn.query_row(
            "SELECT COUNT(*) FROM rag_queue WHERE status='queued'",
            [],
            |row| row.get(0),
        )?;
        let processing_items = conn.query_row(
            "SELECT COUNT(*) FROM rag_queue WHERE status='processing'",
            [],
            |row| row.get(0),
        )?;

        Ok(RagInfoSummary {
            total_chunks,
            indexed_projects: indexed_files, // Now counts unique files, not projects
            queued_items,
            processing_items,
        })
    }

    // ── RAG chunks (FTS5) ──────────────────────────────────────────────

    /// Delete all chunks for a file, then insert new ones.
    /// project_hash is now optional metadata (not part of the key).
    pub fn replace_chunks(&self, source_path: &str, chunks: &[Chunk]) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        let tx = conn.transaction()?;

        tx.execute(
            "DELETE FROM rag_chunks WHERE source_path=?1",
            rusqlite::params![source_path],
        )?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO rag_chunks(id,project_hash,source_path,chunk_index,content,lang,embedding,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;

            for c in chunks {
                stmt.execute(rusqlite::params![
                    c.id,
                    c.project_hash,
                    c.source_path,
                    c.chunk_index,
                    c.content,
                    c.lang,
                    serialize_embedding(c.embedding.as_deref()),
                    c.updated_at
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// FTS5 search over chunk content.
    pub fn search_chunks(
        &self,
        query: &str,
        project_hash: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Chunk>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        // Escape FTS5 special chars minimally
        let fts_query = query.replace('"', "\"\"");

        if let Some(ph) = project_hash {
            let mut stmt = conn.prepare(
                "SELECT c.id,c.project_hash,c.source_path,c.chunk_index,c.content,c.lang,c.embedding,c.updated_at
                 FROM rag_chunks_fts f
                 JOIN rag_chunks c ON c.rowid = f.rowid
                 WHERE rag_chunks_fts MATCH ?1 AND c.project_hash=?2
                 ORDER BY rank LIMIT ?3",
            )?;
            let rows: Vec<Chunk> = stmt
                .query_map(rusqlite::params![fts_query, ph, limit as i64], row_to_chunk)?
                .filter_map(|row| row.ok())
                .collect();
            Ok(rows)
        } else {
            let mut stmt = conn.prepare(
                "SELECT c.id,c.project_hash,c.source_path,c.chunk_index,c.content,c.lang,c.embedding,c.updated_at
                 FROM rag_chunks_fts f
                 JOIN rag_chunks c ON c.rowid = f.rowid
                 WHERE rag_chunks_fts MATCH ?1
                 ORDER BY rank LIMIT ?2",
            )?;
            let rows: Vec<Chunk> = stmt
                .query_map(rusqlite::params![fts_query, limit as i64], row_to_chunk)?
                .filter_map(|row| row.ok())
                .collect();
            Ok(rows)
        }
    }

    pub fn search_chunks_by_embedding(
        &self,
        embedding: &[f32],
        project_hash: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ScoredChunk>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        let rows = if let Some(ph) = project_hash {
            let mut stmt = conn.prepare(
                "SELECT id,project_hash,source_path,chunk_index,content,lang,embedding,updated_at
                 FROM rag_chunks
                 WHERE embedding IS NOT NULL AND project_hash=?1",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![ph], row_to_chunk)?
                .filter_map(|row| row.ok())
                .collect::<Vec<_>>();
            rows
        } else {
            let mut stmt = conn.prepare(
                "SELECT id,project_hash,source_path,chunk_index,content,lang,embedding,updated_at
                 FROM rag_chunks
                 WHERE embedding IS NOT NULL",
            )?;
            let rows = stmt
                .query_map([], row_to_chunk)?
                .filter_map(|row| row.ok())
                .collect::<Vec<_>>();
            rows
        };

        let mut matches: Vec<ScoredChunk> = rows
            .into_iter()
            .filter_map(|chunk| {
                let score = {
                    let candidate = chunk.embedding.as_deref()?;
                    cosine_similarity(embedding, candidate)?
                };
                Some(ScoredChunk { chunk, score })
            })
            .collect();
        matches.sort_by(|left, right| right.score.total_cmp(&left.score));
        matches.truncate(limit);
        Ok(matches)
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

fn row_to_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chunk> {
    let embedding = row
        .get::<_, Option<Vec<u8>>>(6)?
        .map(|bytes| deserialize_embedding(&bytes))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(6, Type::Blob, Box::new(error))
        })?;

    Ok(Chunk {
        id: row.get(0)?,
        project_hash: row.get(1)?,
        source_path: row.get(2)?,
        chunk_index: row.get(3)?,
        content: row.get(4)?,
        lang: row.get(5)?,
        embedding,
        updated_at: row.get(7)?,
    })
}

fn serialize_embedding(embedding: Option<&[f32]>) -> Option<Vec<u8>> {
    embedding.map(|values| {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    })
}

fn deserialize_embedding(bytes: &[u8]) -> std::io::Result<Vec<f32>> {
    let chunks = bytes.chunks_exact(std::mem::size_of::<f32>());
    if !chunks.remainder().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "embedding blob length must be divisible by {}",
                std::mem::size_of::<f32>()
            ),
        ));
    }

    Ok(bytes
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }

    let dot = left
        .iter()
        .zip(right)
        .map(|(lhs, rhs)| lhs * rhs)
        .sum::<f32>();
    let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();

    if left_norm == 0.0 || right_norm == 0.0 {
        None
    } else {
        Some((dot / (left_norm * right_norm)).clamp(-1.0, 1.0))
    }
}

fn row_to_rag_queue_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<RagQueueItem> {
    Ok(RagQueueItem {
        source_path: row.get(0)?,
        status: row.get(1)?,
        queued_at: row.get(2)?,
    })
}
