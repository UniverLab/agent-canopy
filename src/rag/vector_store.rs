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
    pub async fn new(embedding_dimensions: usize) -> Result<Self> {
        let home = dirs::home_dir().context("Could not determine home directory")?;
        Self::open_at(&home.join(DEFAULT_DB_DIR), embedding_dimensions).await
    }

    pub fn default_lancedb_path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Could not determine home directory")?;
        Ok(home.join(DEFAULT_DB_DIR))
    }

    pub async fn open_at(path: &Path, embedding_dimensions: usize) -> Result<Self> {
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

        let table = match connection.open_table(TABLE_NAME).execute().await {
            Ok(existing_table) => {
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
                    let new_table = connection
                        .create_empty_table(TABLE_NAME, schema.clone())
                        .execute()
                        .await
                        .context("Failed to recreate LanceDB table after schema change")?;
                    tracing::info!(
                        "RAG VectorStore: recreated table with {} dimensions",
                        embedding_dimensions
                    );
                    new_table
                } else {
                    tracing::info!("RAG VectorStore: schema OK, reusing existing table");
                    existing_table
                }
            }
            Err(open_error) => {
                tracing::info!(
                    "RAG VectorStore: table not found ({}), creating fresh with {} dims",
                    open_error,
                    embedding_dimensions
                );
                connection
                    .create_empty_table(TABLE_NAME, schema.clone())
                    .execute()
                    .await
                    .with_context(|| {
                        format!("Failed to create LanceDB table {TABLE_NAME} after open error: {open_error}")
                    })?
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
        let batches: Vec<RecordBatch> = self
            .table
            .query()
            .select(Select::columns(&["file_path"]))
            .execute()
            .await
            .context("Failed to query LanceDB for unique paths")?
            .try_collect()
            .await
            .context("Failed to collect LanceDB path query results")?;

        let mut paths = std::collections::HashSet::new();
        for batch in &batches {
            if let Some(col) = batch.column_by_name("file_path") {
                if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    for i in 0..arr.len() {
                        paths.insert(arr.value(i).to_string());
                    }
                }
            }
        }
        Ok(paths.into_iter().collect())
    }

    /// Return a map of `file_path → chunk_count` for every indexed file.
    pub async fn count_chunks_per_file(&self) -> Result<std::collections::HashMap<String, usize>> {
        let batches: Vec<RecordBatch> = self
            .table
            .query()
            .select(Select::columns(&["file_path"]))
            .execute()
            .await
            .context("Failed to query LanceDB for chunk counts")?
            .try_collect()
            .await
            .context("Failed to collect LanceDB chunk-count results")?;

        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for batch in &batches {
            if let Some(col) = batch.column_by_name("file_path") {
                if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    for i in 0..arr.len() {
                        *counts.entry(arr.value(i).to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
        Ok(counts)
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
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4)
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
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4)
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
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4)
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
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4)
            .await
            .unwrap();
        let embedder = MockEmbeddingClient::new(4);
        let content = "# Alpha\n\nalpha beta gamma\n\n## Beta\n\ndelta epsilon zeta";

        for semantic_chunk in chunk_semantic(content, "markdown", 0.4) {
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

        let query = embedder.embed("alpha beta gamma").unwrap();
        let results = store.search_similar(&query, 1).await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("Alpha"));
        assert_eq!(results[0].file_path, "/docs/guide.md");
    }

    #[tokio::test]
    async fn count_chunks_returns_total_and_unique_paths() {
        let temp_dir = TempDir::new().unwrap();
        let store = VectorStore::open_at(&VectorStore::path_for_tests(temp_dir.path()), 4)
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
}
