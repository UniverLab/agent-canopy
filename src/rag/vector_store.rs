#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arrow_array::types::Float32Type;
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Float64Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::{connect, Connection, Table};

const DEFAULT_DB_DIR: &str = ".canopy/rag/vectors.lancedb";
const TABLE_NAME: &str = "chunks";

/// Default ceiling on LanceDB's index cache, in entries (see
/// `lancedb::connection::OpenTableBuilder::index_cache_size`, which converts
/// entries to bytes at ~20 MiB/entry — the same conversion `lance::dataset`
/// uses internally for its now-deprecated entry-count API). Left unset,
/// LanceDB defaults to `lance::dataset::DEFAULT_INDEX_CACHE_SIZE` = 6 GiB.
///
/// Measured on this repo's own indexed workspace (~1,900 chunks, 384-dim
/// embeddings, no `create_index()` call so search is a brute-force KNN
/// scan): with no vector index ever built, the index cache holds nothing,
/// so 64 entries (~1.25 GiB ceiling) vs. LanceDB's unbounded-by-default
/// 6 GiB showed no measurable difference in RSS or query latency — the
/// cache simply isn't populated by this workload today. 64 is kept as the
/// default anyway because it cuts the *worst-case* ceiling by ~80% against
/// LanceDB's built-in default at zero cost to today's brute-force search,
/// and gives headroom for if/when an ANN index is added later.
pub const DEFAULT_INDEX_CACHE_ENTRIES: u32 = 64;

/// Resolve the effective LanceDB index cache bound: the configured value,
/// or `DEFAULT_INDEX_CACHE_ENTRIES` when nothing is configured. Kept as a
/// pure function, separate from the LanceDB calls that consume it, so the
/// resolution itself is directly testable.
fn resolve_index_cache_entries(configured: Option<u32>) -> u32 {
    configured.unwrap_or(DEFAULT_INDEX_CACHE_ENTRIES)
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorChunk {
    pub id: String,
    pub file_path: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub id: String,
    pub file_path: String,
    pub content: String,
    pub created_at: i64,
    pub distance: Option<f32>,
}

pub struct VectorStore {
    _connection: Connection,
    table: Table,
    schema: Arc<Schema>,
    embedding_dimensions: i32,
}

impl VectorStore {
    pub async fn new(
        embedding_dimensions: usize,
        index_cache_entries: Option<u32>,
    ) -> Result<Self> {
        let home = dirs::home_dir().context("Could not determine home directory")?;
        Self::open_at(
            &home.join(DEFAULT_DB_DIR),
            embedding_dimensions,
            index_cache_entries,
        )
        .await
    }

    pub fn default_lancedb_path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Could not determine home directory")?;
        Ok(home.join(DEFAULT_DB_DIR))
    }

    pub async fn open_at(
        path: &Path,
        embedding_dimensions: usize,
        index_cache_entries: Option<u32>,
    ) -> Result<Self> {
        let cache_entries = resolve_index_cache_entries(index_cache_entries);
        let embedding_dimensions = i32::try_from(embedding_dimensions)
            .context("Embedding dimensions exceed supported LanceDB schema size")?;
        if embedding_dimensions <= 0 {
            bail!("Embedding dimensions must be greater than zero");
        }

        tracing::debug!(
            "RAG VectorStore: opening at {} with {} dimensions",
            path.display(),
            embedding_dimensions
        );

        std::fs::create_dir_all(path)
            .with_context(|| format!("Failed to create LanceDB directory at {}", path.display()))?;

        let connection = connect(&path_to_uri(path)?)
            .execute()
            .await
            .with_context(|| format!("Failed to open LanceDB at {}", path.display()))?;
        let schema = chunk_schema(embedding_dimensions);

        let known_tables = connection
            .table_names()
            .execute()
            .await
            .context("Failed to list LanceDB tables")?;

        let table = if !known_tables.iter().any(|name| name == TABLE_NAME) {
            tracing::info!(
                "RAG VectorStore: no existing table found, creating fresh with {} dims",
                embedding_dimensions
            );
            connection
                .create_empty_table(TABLE_NAME, schema.clone())
                .execute()
                .await
                .context("Failed to create LanceDB table")?;
            // create_empty_table's builder has no cache-size knob (there is no
            // index yet on an empty table); reopen once so `self.table` always
            // carries the configured bound, regardless of which branch built it.
            open_table_with_cache(&connection, cache_entries).await?
        } else {
            // The table is listed on disk, so it must not be silently replaced:
            // an open failure here is a transient race (e.g. concurrent manifest
            // rewrite) or genuine corruption, never "table does not exist".
            let existing_table = open_existing_table_with_retry(&connection, cache_entries).await?;

            // Check whether the stored schema matches the requested dimensions.
            // If they differ (e.g. model was changed), drop and recreate the table.
            let stored_dims = embedding_dims_from_table(&existing_table).await;
            tracing::info!(
                "RAG VectorStore: existing table found — stored_dims={:?}, requested={}",
                stored_dims,
                embedding_dimensions
            );
            if stored_dims != Some(embedding_dimensions) {
                tracing::warn!(
                    "RAG VectorStore: schema mismatch (stored={:?} vs requested={}) — dropping and recreating table",
                    stored_dims,
                    embedding_dimensions
                );
                connection
                    .drop_table(TABLE_NAME, &[])
                    .await
                    .context("Failed to drop outdated LanceDB table")?;
                connection
                    .create_empty_table(TABLE_NAME, schema.clone())
                    .execute()
                    .await
                    .context("Failed to recreate LanceDB table after schema change")?;
                tracing::info!(
                    "RAG VectorStore: recreated table with {} dimensions",
                    embedding_dimensions
                );
                open_table_with_cache(&connection, cache_entries).await?
            } else {
                tracing::info!("RAG VectorStore: schema OK, reusing existing table");
                existing_table
            }
        };

        Ok(Self {
            _connection: connection,
            table,
            schema,
            embedding_dimensions,
        })
    }

    pub async fn insert_chunk(&self, chunk: &VectorChunk) -> Result<()> {
        self.validate_embedding(&chunk.embedding)?;
        let batch = chunk_batch(
            self.schema.clone(),
            std::slice::from_ref(chunk),
            self.embedding_dimensions,
        )?;
        self.table.add(batch).execute().await.with_context(|| {
            format!(
                "LanceDB add() failed for chunk {} (file: {}, embedding_dims={})",
                chunk.id,
                chunk.file_path,
                chunk.embedding.len()
            )
        })?;
        Ok(())
    }

    pub async fn search_similar(
        &self,
        query_vec: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        self.validate_embedding(query_vec)?;
        if top_k == 0 {
            return Ok(Vec::new());
        }

        let batches: Vec<RecordBatch> = self
            .table
            .vector_search(query_vec.to_vec())
            .context("Failed to build LanceDB vector search")?
            .limit(top_k)
            .execute()
            .await
            .context("Failed to execute LanceDB vector search")?
            .try_collect()
            .await
            .context("Failed to collect LanceDB search results")?;

        batches
            .iter()
            .flat_map(parse_search_batch)
            .collect::<Result<Vec<_>>>()
    }

    pub async fn delete_by_path(&self, file_path: &str) -> Result<()> {
        let predicate = format!("file_path = '{}'", file_path.replace('\'', "''"));
        self.table
            .delete(&predicate)
            .await
            .with_context(|| format!("Failed to delete chunks for {file_path}"))?;
        Ok(())
    }

    pub async fn count_chunks(&self) -> Result<i64> {
        let count = self
            .table
            .count_rows(None)
            .await
            .context("Failed to count LanceDB rows")?;
        Ok(count as i64)
    }

    pub async fn count_unique_paths(&self) -> Result<i64> {
        let paths = self.list_unique_paths().await?;
        Ok(paths.len() as i64)
    }

    /// Return every distinct `file_path` stored in the vector table.
    /// Used for orphan-chunk reconciliation at startup.
    pub async fn list_unique_paths(&self) -> Result<Vec<String>> {
        let file_paths = self.query_file_paths().await?;
        let paths: std::collections::HashSet<String> = file_paths.into_iter().collect();
        Ok(paths.into_iter().collect())
    }

    /// Return a map of `file_path → chunk_count` for every indexed file.
    pub async fn count_chunks_per_file(&self) -> Result<std::collections::HashMap<String, usize>> {
        let file_paths = self.query_file_paths().await?;
        let mut counts = std::collections::HashMap::new();
        for path in file_paths {
            *counts.entry(path).or_insert(0) += 1;
        }
        Ok(counts)
    }

    /// Query all `file_path` values from the vector table.
    async fn query_file_paths(&self) -> Result<Vec<String>> {
        let batches: Vec<RecordBatch> = self
            .table
            .query()
            .select(Select::columns(&["file_path"]))
            .execute()
            .await
            .context("Failed to query LanceDB for file paths")?
            .try_collect()
            .await
            .context("Failed to collect LanceDB file path results")?;

        let mut paths = Vec::new();
        for batch in &batches {
            if let Some(col) = batch.column_by_name("file_path") {
                if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    for i in 0..arr.len() {
                        paths.push(arr.value(i).to_string());
                    }
                }
            }
        }
        Ok(paths)
    }

    pub fn path_for_tests(base_dir: &Path) -> PathBuf {
        base_dir.join("vectors.lancedb")
    }

    fn validate_embedding(&self, embedding: &[f32]) -> Result<()> {
        if embedding.len() == usize::try_from(self.embedding_dimensions).unwrap_or_default() {
            Ok(())
        } else {
            bail!(
                "Embedding dimensions mismatch: expected {}, got {}",
                self.embedding_dimensions,
                embedding.len()
            )
        }
    }
}

/// Number of attempts to open a table already listed by `table_names()` before
/// giving up. LanceDB's manifest can be transiently rewritten by background
/// cleanup, which makes a concurrent `open_table` fail with a spurious
/// "not found" even though the table is present on disk.
const OPEN_TABLE_MAX_ATTEMPTS: u32 = 3;

/// Open a table that `table_names()` has already confirmed exists, retrying
/// with backoff on failure. Never falls back to creating an empty table:
/// callers must treat a persistent failure as fatal, not as "table missing".
async fn open_existing_table_with_retry(
    connection: &Connection,
    cache_entries: u32,
) -> Result<Table> {
    let mut last_error = None;
    for attempt in 1..=OPEN_TABLE_MAX_ATTEMPTS {
        match connection
            .open_table(TABLE_NAME)
            .index_cache_size(cache_entries)
            .execute()
            .await
        {
            Ok(table) => return Ok(table),
            Err(error) => {
                tracing::warn!(
                    "RAG VectorStore: open_table attempt {}/{} failed: {}",
                    attempt,
                    OPEN_TABLE_MAX_ATTEMPTS,
                    error
                );
                last_error = Some(error);
                if attempt < OPEN_TABLE_MAX_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)))
                        .await;
                }
            }
        }
    }
    Err(last_error.expect("loop runs at least once")).with_context(|| {
        format!(
            "Failed to open existing LanceDB table {TABLE_NAME} after {OPEN_TABLE_MAX_ATTEMPTS} attempts"
        )
    })
}

/// Reopen a table that was just created, applying the configured index
/// cache bound. `create_empty_table`'s builder has no cache-size knob (an
/// empty table has no index to cache yet), so this is the one place that
/// bound gets attached for a freshly created or recreated table.
async fn open_table_with_cache(connection: &Connection, cache_entries: u32) -> Result<Table> {
    connection
        .open_table(TABLE_NAME)
        .index_cache_size(cache_entries)
        .execute()
        .await
        .context("Failed to reopen LanceDB table with configured index cache size")
}

fn path_to_uri(path: &Path) -> Result<String> {
    Ok(path
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize LanceDB path {}", path.display()))?
        .to_string_lossy()
        .to_string())
}

fn chunk_schema(embedding_dimensions: i32) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                embedding_dimensions,
            ),
            false,
        ),
        Field::new("created_at", DataType::Int64, false),
    ]))
}

fn chunk_batch(
    schema: Arc<Schema>,
    chunks: &[VectorChunk],
    embedding_dimensions: i32,
) -> Result<RecordBatch> {
    let ids = StringArray::from_iter_values(chunks.iter().map(|chunk| chunk.id.as_str()));
    let file_paths =
        StringArray::from_iter_values(chunks.iter().map(|chunk| chunk.file_path.as_str()));
    let contents = StringArray::from_iter_values(chunks.iter().map(|chunk| chunk.content.as_str()));
    let created_at = Int64Array::from_iter_values(chunks.iter().map(|chunk| chunk.created_at));
    let embeddings = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        chunks.iter().map(|chunk| {
            Some(
                chunk
                    .embedding
                    .iter()
                    .copied()
                    .map(Some)
                    .collect::<Vec<_>>(),
            )
        }),
        embedding_dimensions,
    );

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(ids),
            Arc::new(file_paths),
            Arc::new(contents),
            Arc::new(embeddings),
            Arc::new(created_at),
        ],
    )
    .context("Failed to build Arrow record batch for LanceDB")
}

fn parse_search_batch(batch: &RecordBatch) -> Vec<Result<SearchResult>> {
    let ids = batch
        .column_by_name("id")
        .and_then(|column| column.as_any().downcast_ref::<StringArray>())
        .context("LanceDB search result is missing id column")
        .cloned();
    let file_paths = batch
        .column_by_name("file_path")
        .and_then(|column| column.as_any().downcast_ref::<StringArray>())
        .context("LanceDB search result is missing file_path column")
        .cloned();
    let contents = batch
        .column_by_name("content")
        .and_then(|column| column.as_any().downcast_ref::<StringArray>())
        .context("LanceDB search result is missing content column")
        .cloned();
    let created_at = batch
        .column_by_name("created_at")
        .and_then(|column| column.as_any().downcast_ref::<Int64Array>())
        .context("LanceDB search result is missing created_at column")
        .cloned();

    match (ids, file_paths, contents, created_at) {
        (Ok(ids), Ok(file_paths), Ok(contents), Ok(created_at)) => (0..batch.num_rows())
            .map(|row| {
                Ok(SearchResult {
                    id: ids.value(row).to_owned(),
                    file_path: file_paths.value(row).to_owned(),
                    content: contents.value(row).to_owned(),
                    created_at: created_at.value(row),
                    distance: distance_at(batch, row),
                })
            })
            .collect(),
        (Err(error), _, _, _)
        | (_, Err(error), _, _)
        | (_, _, Err(error), _)
        | (_, _, _, Err(error)) => vec![Err(error)],
    }
}

fn distance_at(batch: &RecordBatch, row: usize) -> Option<f32> {
    batch.column_by_name("_distance").and_then(|column| {
        column
            .as_any()
            .downcast_ref::<Float32Array>()
            .map(|distances| distances.value(row))
            .or_else(|| {
                column
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .map(|distances| distances.value(row) as f32)
            })
    })
}

/// Extract the embedding vector dimensions from an existing LanceDB table schema.
async fn embedding_dims_from_table(table: &Table) -> Option<i32> {
    let schema = table.schema().await.ok()?;
    let field = schema.field_with_name("embedding").ok()?;
    match field.data_type() {
        DataType::FixedSizeList(_, size) => Some(*size),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::chunker::chunk_semantic;
    use crate::rag::embedding_client::{EmbeddingClient, MockEmbeddingClient};
    use tempfile::TempDir;

    fn chunk(id: &str, file_path: &str, content: &str, embedding: Vec<f32>) -> VectorChunk {
        VectorChunk {
            id: id.to_string(),
            file_path: file_path.to_string(),
            content: content.to_string(),
            embedding,
            created_at: 1_715_000_000,
        }
    }

    #[tokio::test]
    async fn vector_store_inserts_and_searches_chunks() {
        let temp_dir = TempDir::new().unwrap();
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4, None)
            .await
            .unwrap();
        store
            .insert_chunk(&chunk("a", "/docs/a.md", "alpha", vec![1.0, 0.0, 0.0, 0.0]))
            .await
            .unwrap();
        store
            .insert_chunk(&chunk("b", "/docs/b.md", "beta", vec![0.0, 1.0, 0.0, 0.0]))
            .await
            .unwrap();

        let results = store
            .search_similar(&[1.0, 0.0, 0.0, 0.0], 1)
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
        assert_eq!(results[0].file_path, "/docs/a.md");
    }

    #[tokio::test]
    async fn vector_store_deletes_chunks_by_path() {
        let temp_dir = TempDir::new().unwrap();
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4, None)
            .await
            .unwrap();
        store
            .insert_chunk(&chunk("a", "/docs/a.md", "alpha", vec![1.0, 0.0, 0.0, 0.0]))
            .await
            .unwrap();

        store.delete_by_path("/docs/a.md").await.unwrap();

        let results = store
            .search_similar(&[1.0, 0.0, 0.0, 0.0], 5)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn vector_store_rejects_wrong_dimensions() {
        let temp_dir = TempDir::new().unwrap();
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4, None)
            .await
            .unwrap();

        let error = store
            .insert_chunk(&chunk("a", "/docs/a.md", "alpha", vec![1.0, 0.0]))
            .await
            .expect_err("dimension mismatch should fail");

        assert!(error.to_string().contains("Embedding dimensions mismatch"));
    }

    #[tokio::test]
    async fn semantic_chunking_embedding_and_vector_search_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4, None)
            .await
            .unwrap();
        let embedder = MockEmbeddingClient::new(4);
        let content = "# Alpha\n\nalpha beta gamma\n\n## Beta\n\ndelta epsilon zeta";

        // Dissimilar sections stay separate under semantic chunking.
        let semantic_chunks = chunk_semantic(content, "markdown", 0.4);
        assert_eq!(semantic_chunks.len(), 2);
        let first_content = semantic_chunks[0].content.clone();

        for semantic_chunk in semantic_chunks {
            let embedding = embedder.embed(&semantic_chunk.content).unwrap();
            let chunk = VectorChunk {
                id: format!("chunk-{}", semantic_chunk.index),
                file_path: "/docs/guide.md".to_string(),
                content: semantic_chunk.content,
                embedding,
                created_at: 1_715_000_000,
            };
            store.insert_chunk(&chunk).await.unwrap();
        }

        // Querying with a stored chunk's exact text must return that chunk
        // (identical embedding → distance 0). The mock embedder hashes by
        // token position, so anything looser would test luck, not the store.
        let query = embedder.embed(&first_content).unwrap();
        let results = store.search_similar(&query, 1).await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, first_content);
        assert_eq!(results[0].file_path, "/docs/guide.md");
    }

    /// Truncate all regular files under `dir` to 0 bytes (simulates corruption).
    fn corrupt_all_files(dir: &std::path::Path) {
        let entries = std::fs::read_dir(dir).unwrap();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                std::fs::write(&path, []).unwrap();
            } else if path.is_dir() {
                corrupt_all_files(&path);
            }
        }
    }

    #[tokio::test]
    async fn corrupted_store_fails_to_open() {
        let temp_dir = TempDir::new().unwrap();
        let lancedb_path = VectorStore::path_for_tests(temp_dir.path());

        // Create a valid store with data.
        let store = VectorStore::open_at(&lancedb_path, 4, None).await.unwrap();
        store
            .insert_chunk(&chunk("a", "/test.md", "hello", vec![1.0, 0.0, 0.0, 0.0]))
            .await
            .unwrap();
        drop(store);

        // Corrupt the store by truncating all files to 0 bytes.
        corrupt_all_files(&lancedb_path);

        // Opening the corrupted store should fail.
        let result = VectorStore::open_at(&lancedb_path, 4, None).await;
        assert!(result.is_err(), "corrupted store should fail to open");
    }

    #[tokio::test]
    async fn open_failure_on_known_existing_table_propagates_instead_of_recreating() {
        let temp_dir = TempDir::new().unwrap();
        let lancedb_path = VectorStore::path_for_tests(temp_dir.path());

        // Create a valid store with data.
        let store = VectorStore::open_at(&lancedb_path, 4, None).await.unwrap();
        store
            .insert_chunk(&chunk("a", "/test.md", "hello", vec![1.0, 0.0, 0.0, 0.0]))
            .await
            .unwrap();
        drop(store);

        // Corrupt the store by truncating all files to 0 bytes.
        corrupt_all_files(&lancedb_path);

        // The table is still listed on disk even though its manifest is
        // corrupt — table_names() is a plain directory scan, independent of
        // manifest integrity. This is exactly the condition that used to make
        // VectorStore::new() treat the table as "does not exist" and silently
        // overwrite it with an empty one.
        let connection = connect(&path_to_uri(&lancedb_path).unwrap())
            .execute()
            .await
            .unwrap();
        let known_tables = connection.table_names().execute().await.unwrap();
        assert!(known_tables.iter().any(|name| name == TABLE_NAME));
        drop(connection);

        // Opening must propagate the failure, not fall back to creating an
        // empty table — that would silently discard the existing row.
        let result = VectorStore::open_at(&lancedb_path, 4, None).await;
        assert!(
            result.is_err(),
            "open failure on a known-existing table must propagate, not recreate it empty"
        );
    }

    #[tokio::test]
    async fn corrupted_store_recovers_after_purge() {
        let temp_dir = TempDir::new().unwrap();
        let lancedb_path = VectorStore::path_for_tests(temp_dir.path());

        // Create a valid store with data.
        let store = VectorStore::open_at(&lancedb_path, 4, None).await.unwrap();
        store
            .insert_chunk(&chunk("a", "/test.md", "hello", vec![1.0, 0.0, 0.0, 0.0]))
            .await
            .unwrap();
        drop(store);

        // Corrupt the store by truncating all files to 0 bytes.
        corrupt_all_files(&lancedb_path);

        // Opening the corrupted store should fail.
        assert!(VectorStore::open_at(&lancedb_path, 4, None).await.is_err());

        // Purge (delete) the corrupted directory — simulates wipe_lancedb_dir.
        std::fs::remove_dir_all(&lancedb_path).unwrap();

        // Opening after purge should succeed with a fresh (empty) table.
        let store = VectorStore::open_at(&lancedb_path, 4, None).await.unwrap();
        assert_eq!(store.count_chunks().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn schema_mismatch_drops_and_recreates_table() {
        let temp_dir = TempDir::new().unwrap();
        let lancedb_path = VectorStore::path_for_tests(temp_dir.path());

        // Create a store with 4-dimensional embeddings and a row.
        let store = VectorStore::open_at(&lancedb_path, 4, None).await.unwrap();
        store
            .insert_chunk(&chunk("a", "/test.md", "hello", vec![1.0, 0.0, 0.0, 0.0]))
            .await
            .unwrap();
        assert_eq!(store.count_chunks().await.unwrap(), 1);
        drop(store);

        // Reopening with a different embedding dimension (e.g. model change)
        // must drop and recreate the table, ending up empty with the new schema.
        let store = VectorStore::open_at(&lancedb_path, 8, None).await.unwrap();
        assert_eq!(store.count_chunks().await.unwrap(), 0);
        store
            .insert_chunk(&chunk(
                "b",
                "/test2.md",
                "world",
                vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ))
            .await
            .unwrap();
        assert_eq!(store.count_chunks().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn missing_directory_creates_fresh_table() {
        let temp_dir = TempDir::new().unwrap();
        let lancedb_path = temp_dir.path().join("nonexistent").join("vectors.lancedb");

        // open_at should create the directory and table from scratch.
        let store = VectorStore::open_at(&lancedb_path, 4, None).await.unwrap();
        assert_eq!(store.count_chunks().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn count_chunks_returns_total_and_unique_paths() {
        let temp_dir = TempDir::new().unwrap();
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4, None)
            .await
            .unwrap();
        store
            .insert_chunk(&chunk("a", "/docs/a.md", "alpha", vec![1.0, 0.0, 0.0, 0.0]))
            .await
            .unwrap();
        store
            .insert_chunk(&chunk("b", "/docs/a.md", "beta", vec![0.0, 1.0, 0.0, 0.0]))
            .await
            .unwrap();
        store
            .insert_chunk(&chunk("c", "/docs/b.md", "gamma", vec![0.0, 0.0, 1.0, 0.0]))
            .await
            .unwrap();

        assert_eq!(store.count_chunks().await.unwrap(), 3);
        assert_eq!(store.count_unique_paths().await.unwrap(), 2);
    }

    // ── C9: vector cache bound ──────────────────────────────────────

    #[test]
    fn resolve_index_cache_entries_applies_configured_bound() {
        assert_eq!(resolve_index_cache_entries(Some(7)), 7);
    }

    #[test]
    fn resolve_index_cache_entries_applies_default_when_unconfigured() {
        assert_eq!(
            resolve_index_cache_entries(None),
            DEFAULT_INDEX_CACHE_ENTRIES
        );
    }

    #[tokio::test]
    async fn open_at_accepts_a_configured_cache_bound() {
        let temp_dir = TempDir::new().unwrap();
        // A value LanceDB actually receives via `.index_cache_size()` on both
        // the create path and the reuse-on-reopen path — an unsupported or
        // mis-threaded value would surface as an Err here, not silently.
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4, Some(1))
            .await
            .unwrap();
        assert_eq!(store.count_chunks().await.unwrap(), 0);

        // Reopening the same on-disk table exercises the "existing table"
        // path, which threads the bound through a different call site.
        let reopened =
            VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4, Some(1))
                .await
                .unwrap();
        assert_eq!(reopened.count_chunks().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn search_results_identical_with_and_without_cache_bound() {
        let bounded_dir = TempDir::new().unwrap();
        let unbounded_dir = TempDir::new().unwrap();
        let bounded =
            VectorStore::open_at(&VectorStore::path_for_tests(bounded_dir.path()), 4, Some(2))
                .await
                .unwrap();
        let unbounded =
            VectorStore::open_at(&VectorStore::path_for_tests(unbounded_dir.path()), 4, None)
                .await
                .unwrap();

        for (id, embedding) in [
            ("a", vec![1.0, 0.0, 0.0, 0.0]),
            ("b", vec![0.0, 1.0, 0.0, 0.0]),
            ("c", vec![0.0, 0.0, 1.0, 0.0]),
        ] {
            let file_path = format!("/docs/{id}.md");
            bounded
                .insert_chunk(&chunk(id, &file_path, id, embedding.clone()))
                .await
                .unwrap();
            unbounded
                .insert_chunk(&chunk(id, &file_path, id, embedding))
                .await
                .unwrap();
        }

        let query = [0.9, 0.1, 0.0, 0.0];
        let bounded_results = bounded.search_similar(&query, 3).await.unwrap();
        let unbounded_results = unbounded.search_similar(&query, 3).await.unwrap();

        // A cache limit changes memory and latency, never answers.
        assert_eq!(bounded_results, unbounded_results);
    }

    #[tokio::test]
    async fn search_stays_correct_when_index_outgrows_the_cache() {
        let tiny_cache_dir = TempDir::new().unwrap();
        let unbounded_dir = TempDir::new().unwrap();
        // Cache bound of 1 entry — deliberately smaller than what a real
        // index over this many chunks would need — against a store opened
        // with no bound at all (today's un-configured behavior).
        let tiny_cache = VectorStore::open_at(
            &VectorStore::path_for_tests(tiny_cache_dir.path()),
            4,
            Some(1),
        )
        .await
        .unwrap();
        let unbounded =
            VectorStore::open_at(&VectorStore::path_for_tests(unbounded_dir.path()), 4, None)
                .await
                .unwrap();

        for i in 0..50 {
            let id = format!("chunk-{i}");
            let file_path = format!("/docs/{id}.md");
            // Sweep the embedding across the 4 dimensions so results have a
            // well-defined nearest-neighbor ranking to compare.
            let mut embedding = vec![0.0f32; 4];
            embedding[i % 4] = 1.0;
            embedding[(i + 1) % 4] = 0.5 - (i as f32 * 0.001);
            tiny_cache
                .insert_chunk(&chunk(&id, &file_path, &id, embedding.clone()))
                .await
                .unwrap();
            unbounded
                .insert_chunk(&chunk(&id, &file_path, &id, embedding))
                .await
                .unwrap();
        }

        let query = [1.0, 0.5, 0.0, 0.0];
        let tiny_cache_results = tiny_cache.search_similar(&query, 10).await.unwrap();
        let unbounded_results = unbounded.search_similar(&query, 10).await.unwrap();

        assert_eq!(tiny_cache_results, unbounded_results);
        assert_eq!(tiny_cache_results.len(), 10);
    }
}
