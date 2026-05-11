#![allow(dead_code)]
//! `IngestionManager` — async queue + background worker for personal RAG indexing.
//!
//! Indexes only `.md`, `.mdx`, and `.pdf` files from the personal RAG root
//! (`~/.canopy/rag/` by default). Uses LanceDB as the vector search backend.

use std::collections::{HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use crate::application::ports::StateRepository;
use crate::db::Database;
use crate::rag::chunker::{chunk_semantic, detect_lang, SemanticChunk};
use crate::rag::embedding_client::{client_from_config, model_dimensions, EmbeddingClient};
use crate::rag::vector_store::{VectorChunk, VectorStore};

const QUEUE_MAX: usize = 10_000;
const FILE_MAX_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

struct Queue {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl Queue {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    fn push(&mut self, path: &str) -> bool {
        let key = path.to_owned();
        if self.set.contains(&key) {
            self.order.retain(|k| k != &key);
        }
        if self.order.len() >= QUEUE_MAX {
            return false;
        }
        self.order.push_back(key.clone());
        self.set.insert(key);
        true
    }

    fn pop(&mut self) -> Option<String> {
        let item = self.order.pop_front()?;
        self.set.remove(&item);
        Some(item)
    }
}

pub struct IngestionManager {
    db: Arc<Database>,
    data_dir: PathBuf,
    queue: Arc<Mutex<Queue>>,
    notify: Arc<Notify>,
    _personal_watcher: std::sync::Mutex<Option<RecommendedWatcher>>,
    /// Cached embedding client keyed by model id so we load the ONNX model once.
    cached_client: Mutex<Option<(String, Arc<dyn EmbeddingClient>)>>,
}

impl IngestionManager {
    pub fn new(db: Arc<Database>, data_dir: PathBuf) -> Self {
        crate::rag::ragignore::ensure_ragignore(&data_dir);
        Self {
            db,
            data_dir,
            queue: Arc::new(Mutex::new(Queue::new())),
            notify: Arc::new(Notify::new()),
            _personal_watcher: std::sync::Mutex::new(None),
            cached_client: Mutex::new(None),
        }
    }

    pub async fn enqueue(&self, source_path: &str) -> bool {
        let mut q = self.queue.lock().await;
        let ok = q.push(source_path);
        if ok {
            let now = chrono::Utc::now().timestamp();
            if let Err(e) = self.db.enqueue_rag_item(source_path, now) {
                tracing::warn!("RAG queue state error {source_path}: {e}");
            }
            self.notify.notify_one();
        }
        ok
    }

    pub async fn queue_len(&self) -> usize {
        self.queue.lock().await.len()
    }

    /// Clear both the in-memory queue and the DB queue (used on model change).
    pub async fn clear_queue(&self) {
        let mut q = self.queue.lock().await;
        q.order.clear();
        q.set.clear();
        if let Err(e) = self.db.clear_rag_queue() {
            tracing::warn!("RAG: failed to clear DB queue: {e}");
        }
    }

    /// Expose the underlying database for state queries (e.g. model change checks).
    pub(crate) fn db(&self) -> &Arc<Database> {
        &self.db
    }

    pub fn db_pending_queue(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .db
            .list_rag_queue(10_000)?
            .into_iter()
            .map(|i| i.source_path)
            .collect())
    }

    pub fn start_personal_watcher(self: Arc<Self>, personal_roots: &[PathBuf]) {
        if personal_roots.is_empty() {
            return;
        }

        let rt = tokio::runtime::Handle::current();
        let db = Arc::clone(&self.db);
        let queue = Arc::clone(&self.queue);
        let notify_handle = Arc::clone(&self.notify);
        let roots = personal_roots.to_vec();
        let data_dir = self.data_dir.clone();

        let patterns = crate::rag::ragignore::load_patterns(&data_dir);

        let mut watcher = match RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                let Ok(event) = res else { return };
                for path in &event.paths {
                    let matching_root = roots.iter().find(|r| path.starts_with(r));
                    let Some(root) = matching_root else { continue };
                    if crate::rag::ragignore::is_ignored(path, root, &patterns) {
                        continue;
                    }
                    let path_str = path.to_string_lossy().to_string();
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            if detect_lang(&path_str).is_some() {
                                tracing::info!(
                                    "RAG watcher: queuing '{}' for indexing ({:?})",
                                    path_str,
                                    event.kind
                                );
                                let q = Arc::clone(&queue);
                                let n = Arc::clone(&notify_handle);
                                let p = path_str.clone();
                                let db2 = Arc::clone(&db);
                                rt.spawn(async move {
                                    let now = chrono::Utc::now().timestamp();
                                    let ok = {
                                        let mut lock = q.lock().await;
                                        lock.push(&p)
                                    };
                                    if ok {
                                        let _ = db2.enqueue_rag_item(&p, now);
                                        n.notify_one();
                                    }
                                });
                            }
                        }
                        EventKind::Remove(_) => {
                            tracing::info!("RAG watcher: '{}' removed — purging chunks", path_str);
                            let data_dir2 = data_dir.clone();
                            let db3 = Arc::clone(&db);
                            let p = path_str;
                            rt.spawn(async move {
                                purge_vector_chunks(&data_dir2, &p, Some(&db3)).await;
                            });
                        }
                        _ => {}
                    }
                }
            },
            Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("Personal RAG watcher creation failed: {e}");
                return;
            }
        };

        let mut watched = 0usize;
        for root in personal_roots {
            if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
                tracing::warn!("Personal RAG watcher could not watch {:?}: {e}", root);
            } else {
                tracing::info!("Personal RAG watcher active on {:?}", root);
                watched += 1;
            }
        }

        if watched > 0 {
            if let Ok(mut guard) = self._personal_watcher.lock() {
                *guard = Some(watcher);
            }
        }
    }

    pub fn start(self: Arc<Self>) -> tokio_util::sync::CancellationToken {
        let ct = tokio_util::sync::CancellationToken::new();
        let ct_child = ct.child_token();
        tokio::spawn(async move {
            self.run(ct_child).await;
        });
        ct
    }

    /// Scan the vector store for chunks whose source file no longer exists on disk
    /// and delete them. Run once at startup to clean up any orphans left from a
    /// previous session where the daemon was offline during file deletions.
    pub async fn reconcile_orphan_chunks(&self) {
        let config = crate::domain::canopy_config::CanopyConfig::load(&self.data_dir);
        let Some(store) = open_vector_store(&config).await else {
            return;
        };
        let paths = match store.list_unique_paths().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("RAG reconcile: failed to list indexed paths: {e:#}");
                return;
            }
        };
        if paths.is_empty() {
            return;
        }
        tracing::info!(
            "RAG reconcile: checking {} indexed path(s) for orphan chunks",
            paths.len()
        );
        let mut purged = 0usize;
        for path in &paths {
            if !std::path::Path::new(path).exists() {
                tracing::info!(
                    "RAG reconcile: '{}' no longer exists — purging chunks",
                    path
                );
                if let Err(e) = store.delete_by_path(path).await {
                    tracing::warn!("RAG reconcile: failed to purge '{}': {e:#}", path);
                } else {
                    purged += 1;
                    let _ = self.db.log_rag_event(
                        path,
                        "deleted",
                        Some("file no longer on disk — orphan chunks purged at startup"),
                        chrono::Utc::now().timestamp(),
                    );
                }
            }
        }
        if purged > 0 {
            tracing::info!("RAG reconcile: removed chunks for {purged} deleted file(s)");
        } else {
            tracing::debug!("RAG reconcile: no orphan chunks found");
        }
    }

    async fn run(&self, ct: tokio_util::sync::CancellationToken) {
        loop {
            tokio::select! {
                _ = ct.cancelled() => break,
                _ = self.notify.notified() => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    self.drain_queue(&ct).await;
                }
            }
        }
    }

    fn is_paused(&self) -> bool {
        self.db.get_state("rag_paused").ok().flatten().as_deref() == Some("1")
    }

    async fn drain_queue(&self, ct: &tokio_util::sync::CancellationToken) {
        let initial_count = self.queue.lock().await.len();
        if initial_count > 0 {
            crate::domain::notification::send_notification(
                "Canopy — RAG indexing",
                &format!("Indexing queue active: {initial_count} file(s) pending"),
            );
        }

        let mut processed = 0usize;
        let mut skipped = 0usize;
        let mut indexed_paths: Vec<String> = Vec::with_capacity(initial_count);

        loop {
            if !self.wait_while_paused(ct).await {
                return;
            }

            let Some(source_path) = self.queue.lock().await.pop() else {
                break;
            };

            let now = chrono::Utc::now().timestamp();
            let _ = self.db.mark_rag_item_processing(&source_path, now);

            match self.index_file(&source_path).await {
                Ok(()) => {
                    processed += 1;
                    indexed_paths.push(source_path.clone());
                    let _ = self.db.remove_rag_item(&source_path);
                    let _ = self.db.log_rag_event(
                        &source_path,
                        "indexed",
                        None,
                        chrono::Utc::now().timestamp(),
                    );
                }
                Err(e) => {
                    tracing::error!("RAG index error {source_path}: {e:#}");
                    skipped += 1;
                    let _ = self.db.remove_rag_item(&source_path);
                    let error_detail = format!("{e:#}");
                    let _ = self.db.log_rag_event(
                        &source_path,
                        "error",
                        Some(&error_detail),
                        chrono::Utc::now().timestamp(),
                    );
                    // Immediate per-file error notification so the user knows right away.
                    let filename = std::path::Path::new(&source_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| source_path.clone());
                    crate::domain::notification::send_notification(
                        "Canopy — RAG indexing error",
                        &format!("{filename} could not be indexed\nCause: {error_detail}"),
                    );
                }
            }
        }

        if processed > 0 {
            let dir_note = indexing_dir_summary(&indexed_paths);
            tracing::info!("Personal RAG: indexed {processed} file(s){dir_note}");
            crate::domain::notification::send_notification(
                "Canopy — RAG indexing",
                &format!("{processed} file(s) indexed{dir_note}"),
            );
        }
        if skipped > 0 {
            tracing::warn!("Personal RAG: {skipped} file(s) failed — check logs above");
        }
    }

    async fn wait_while_paused(&self, ct: &tokio_util::sync::CancellationToken) -> bool {
        while self.is_paused() {
            tokio::select! {
                _ = ct.cancelled() => return false,
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            }
        }
        !ct.is_cancelled()
    }

    /// Return a cached embedding client, creating it (in a blocking task) if needed.
    /// If the configured model changed since last call, the client is recreated.
    async fn get_embedding_client(
        &self,
        config: &crate::domain::canopy_config::CanopyConfig,
    ) -> anyhow::Result<Arc<dyn EmbeddingClient>> {
        let model_id = config.embeddings_model.trim().to_string();
        let mut guard = self.cached_client.lock().await;

        if let Some((cached_model, client)) = guard.as_ref() {
            if *cached_model == model_id {
                return Ok(Arc::clone(client));
            }
        }

        // Load the model (potentially heavy for local ONNX models) off the async executor.
        tracing::info!("RAG: loading embedding client for model '{model_id}'");
        let config_clone = config.clone();
        let client: Arc<dyn EmbeddingClient> =
            tokio::task::spawn_blocking(move || client_from_config(&config_clone).map(Arc::from))
                .await
                .map_err(|e| anyhow::anyhow!("Embedding client task panicked: {e}"))??;

        *guard = Some((model_id.clone(), Arc::clone(&client)));
        tracing::info!("RAG: embedding client loaded and cached for model '{model_id}'");
        Ok(client)
    }

    async fn index_file(&self, source_path: &str) -> anyhow::Result<()> {
        let path = std::path::Path::new(source_path);

        if !path.exists() {
            purge_vector_chunks(&self.data_dir, source_path, Some(&self.db)).await;
            return Ok(());
        }

        let meta = std::fs::metadata(path)?;
        if meta.len() > FILE_MAX_BYTES {
            tracing::debug!("Personal RAG: skipping large file {source_path}");
            return Ok(());
        }

        let Some(lang) = detect_lang(source_path) else {
            return Ok(());
        };

        let content = extract_file_content(path, lang)?;
        if content.trim().is_empty() {
            tracing::warn!("RAG: skipping {source_path} — content is empty after extraction");
            return Ok(());
        }
        tracing::debug!(
            "RAG index_file: {source_path} — extracted {} bytes (lang={lang})",
            content.len()
        );
        let now = chrono::Utc::now().timestamp();
        let config = crate::domain::canopy_config::CanopyConfig::load(&self.data_dir);

        let threshold = config.similarity_threshold;
        let Some(semantic_chunks) =
            build_semantic_chunks(content.clone(), lang.to_owned(), threshold, source_path).await
        else {
            return Ok(());
        };
        tracing::debug!(
            "RAG index_file: {source_path} — {} semantic chunk(s) produced",
            semantic_chunks.len()
        );
        let embedding_client = match self.get_embedding_client(&config).await {
            Ok(client) => {
                tracing::debug!("RAG index_file: embedding client ready for {source_path}");
                client
            }
            Err(error) => {
                tracing::error!(
                    "RAG: cannot index {source_path} — embedding client unavailable: {error:#}"
                );
                return Err(error);
            }
        };

        let vector_chunks =
            embed_semantic_chunks(embedding_client, semantic_chunks, source_path, now).await;

        if vector_chunks.is_empty() {
            tracing::error!(
                "RAG: no chunks were embedded for {source_path} — file will not be indexed"
            );
            return Ok(());
        }

        tracing::info!(
            "RAG index_file: {source_path} — {} chunk(s) embedded, pushing to vector store",
            vector_chunks.len()
        );
        sync_vector_store(&config, source_path, &vector_chunks).await?;
        Ok(())
    }
}

async fn build_semantic_chunks(
    content: String,
    lang: String,
    threshold: f32,
    source_path: &str,
) -> Option<Vec<SemanticChunk>> {
    let chunk_result = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            chunk_semantic(&content, &lang, threshold)
        }))
    })
    .await;

    match chunk_result {
        Ok(Ok(chunks)) => Some(chunks),
        Ok(Err(panic)) => {
            tracing::error!("RAG chunker panic for {source_path}: {panic:?}");
            None
        }
        Err(join_err) => {
            tracing::error!("RAG chunker task failed for {source_path}: {join_err}");
            None
        }
    }
}

async fn embed_semantic_chunks(
    embedding_client: Arc<dyn EmbeddingClient>,
    semantic_chunks: Vec<SemanticChunk>,
    source_path: &str,
    created_at: i64,
) -> Vec<VectorChunk> {
    let mut vector_chunks = Vec::with_capacity(semantic_chunks.len());

    for chunk in semantic_chunks {
        let content = chunk.content;
        let chunk_id = Uuid::new_v4().to_string();
        let client = Arc::clone(&embedding_client);
        let content_for_embedding = content.clone();
        let embedding_result =
            tokio::task::spawn_blocking(move || client.embed(&content_for_embedding)).await;

        let embedding = match embedding_result {
            Ok(Ok(values)) => {
                tracing::debug!(
                    "RAG: embedded chunk {} of {source_path} → {} dims",
                    chunk.index,
                    values.len()
                );
                values
            }
            Ok(Err(error)) => {
                tracing::error!(
                    "RAG embedding failed for {source_path} chunk {}: {error}",
                    chunk.index
                );
                continue;
            }
            Err(error) => {
                tracing::error!(
                    "RAG embedding task panicked for {source_path} chunk {}: {error}",
                    chunk.index
                );
                continue;
            }
        };

        vector_chunks.push(VectorChunk {
            id: chunk_id,
            file_path: source_path.to_owned(),
            content,
            embedding,
            created_at,
        });
    }

    vector_chunks
}

async fn sync_vector_store(
    config: &crate::domain::canopy_config::CanopyConfig,
    source_path: &str,
    chunks: &[VectorChunk],
) -> anyhow::Result<()> {
    let model = config.embeddings_model.trim();
    tracing::info!(
        "RAG sync_vector_store: {} chunk(s) for {source_path} (model={})",
        chunks.len(),
        model
    );

    let Some(store) = open_vector_store(config).await else {
        anyhow::bail!("RAG vector store unavailable — check model config");
    };

    if let Err(error) = store.delete_by_path(source_path).await {
        tracing::warn!("RAG vector cleanup error {source_path}: {error:#}");
        // Non-fatal — continue inserting fresh chunks even if delete failed.
    }

    let mut ok = 0usize;
    let mut fail = 0usize;
    for chunk in chunks {
        match store.insert_chunk(chunk).await {
            Ok(()) => ok += 1,
            Err(error) => {
                tracing::error!(
                    "RAG insert failed for {source_path} chunk {}: {error:#}",
                    chunk.id
                );
                fail += 1;
            }
        }
    }

    tracing::info!("RAG sync_vector_store: {source_path} — {ok} inserted, {fail} failed");

    if fail > 0 && ok == 0 {
        anyhow::bail!("All {fail} chunk(s) failed to insert for {source_path} — see errors above");
    }
    Ok(())
}

async fn purge_vector_chunks(data_dir: &Path, source_path: &str, db: Option<&crate::db::Database>) {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);
    let Some(store) = open_vector_store(&config).await else {
        tracing::warn!(
            "RAG purge: vector store unavailable — chunks for '{}' may be orphaned",
            source_path
        );
        return;
    };

    match store.delete_by_path(source_path).await {
        Ok(()) => {
            tracing::info!("RAG purge: removed chunks for '{}'", source_path);
            if let Some(db) = db {
                let _ = db.log_rag_event(
                    source_path,
                    "deleted",
                    Some("file removed — watcher triggered chunk purge"),
                    chrono::Utc::now().timestamp(),
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                "RAG purge: failed to remove chunks for '{}': {error:#}",
                source_path
            );
        }
    }
}

fn extract_file_content(path: &Path, lang: &str) -> anyhow::Result<String> {
    if lang == "text" && path.extension().and_then(|e| e.to_str()) == Some("pdf") {
        extract_pdf_text(path)
    } else {
        Ok(std::fs::read_to_string(path)?)
    }
}

/// Build a short directory annotation for the indexing summary notification.
/// If all indexed files share the same parent directory, returns " in <dir>".
/// If they span multiple directories, returns " across <n> dirs".
/// Returns an empty string when the list is empty.
fn indexing_dir_summary(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let dirs: std::collections::HashSet<String> = paths
        .iter()
        .filter_map(|p| {
            std::path::Path::new(p)
                .parent()
                .map(|d| d.to_string_lossy().to_string())
        })
        .collect();
    match dirs.len() {
        0 => String::new(),
        1 => {
            let dir = dirs.into_iter().next().unwrap_or_default();
            let leaf = dir.rsplit('/').next().unwrap_or("?");
            format!(" in {leaf}")
        }
        n => format!(" across {n} dirs"),
    }
}

fn extract_pdf_text(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    pdf_extract::extract_text_from_mem(&buffer)
        .map_err(|e| anyhow::anyhow!("PDF text extraction failed for {}: {e}", path.display()))
}

async fn open_vector_store(
    config: &crate::domain::canopy_config::CanopyConfig,
) -> Option<VectorStore> {
    let model = config.embeddings_model.trim();
    if model.is_empty() {
        return None;
    }

    let dimensions = match model_dimensions(model) {
        Ok(dimensions) => {
            tracing::info!("RAG open_vector_store: model='{model}' dimensions={dimensions}");
            dimensions
        }
        Err(error) => {
            tracing::warn!("RAG vector store unavailable for model {model}: {error:#}");
            return None;
        }
    };

    match VectorStore::new(dimensions).await {
        Ok(store) => {
            tracing::info!("RAG open_vector_store: store opened OK");
            Some(store)
        }
        Err(error) => {
            tracing::warn!("RAG vector store open error: {error:#}");
            None
        }
    }
}

/// Delete the entire LanceDB directory so the next `open_vector_store` starts fresh.
/// Used when the embeddings model is changed so stale vectors don't pollute results.
pub async fn wipe_lancedb(db: &Database) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home dir"))?;
    let lancedb_path = home.join(".canopy/rag/vectors.lancedb");
    if lancedb_path.exists() {
        tokio::fs::remove_dir_all(&lancedb_path)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to wipe LanceDB at {}: {e}", lancedb_path.display())
            })?;
        tracing::info!(
            "RAG: wiped LanceDB at {} (model change)",
            lancedb_path.display()
        );
    }
    // Clear rag_file_events so the report starts clean for the new model.
    let _ = db.clear_rag_file_events();
    Ok(())
}
