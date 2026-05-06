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
use lancedb::query::{ExecutableQuery, QueryBase};
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

    pub async fn open_at(path: &Path, embedding_dimensions: usize) -> Result<Self> {
        let embedding_dimensions = i32::try_from(embedding_dimensions)
            .context("Embedding dimensions exceed supported LanceDB schema size")?;
        if embedding_dimensions <= 0 {
            bail!("Embedding dimensions must be greater than zero");
        }

        std::fs::create_dir_all(path)
            .with_context(|| format!("Failed to create LanceDB directory at {}", path.display()))?;

        let connection = connect(&path_to_uri(path)?)
            .execute()
            .await
            .with_context(|| format!("Failed to open LanceDB at {}", path.display()))?;
        let schema = chunk_schema(embedding_dimensions);
        let table = match connection.open_table(TABLE_NAME).execute().await {
            Ok(table) => table,
            Err(open_error) => connection
                .create_empty_table(TABLE_NAME, schema.clone())
                .execute()
                .await
                .with_context(|| {
                    format!(
                        "Failed to create LanceDB table {TABLE_NAME} after open error: {open_error}"
                    )
                })?,
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
        self.table
            .add(batch)
            .execute()
            .await
            .context("Failed to insert chunk into LanceDB")?;
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
