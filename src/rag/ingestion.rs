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
use crate::rag::chunker::{chunk_semantic, detect_lang};
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
                            let data_dir2 = data_dir.clone();
                            let p = path_str;
                            rt.spawn(async move {
                                purge_vector_chunks(&data_dir2, &p).await;
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
        let mut processed = 0usize;

        loop {
            if !self.wait_while_paused(ct).await {
                return;
            }

            let Some(source_path) = self.queue.lock().await.pop() else {
                break;
            };

            let now = chrono::Utc::now().timestamp();
            let _ = self.db.mark_rag_item_processing(&source_path, now);

            if let Err(e) = self.index_file(&source_path).await {
                tracing::warn!("RAG index error {source_path}: {e}");
            } else {
                processed += 1;
            }
            let _ = self.db.remove_rag_item(&source_path);
        }

        if processed > 0 {
            tracing::info!("Personal RAG: indexed {processed} file(s)");
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

    async fn index_file(&self, source_path: &str) -> anyhow::Result<()> {
        let path = std::path::Path::new(source_path);

        if !path.exists() {
            purge_vector_chunks(&self.data_dir, source_path).await;
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
        let now = chrono::Utc::now().timestamp();
        let config = crate::domain::canopy_config::CanopyConfig::load(&self.data_dir);

        let content_owned = content.clone();
        let lang_owned = lang.to_owned();
        let threshold = config.similarity_threshold;
        let chunk_result = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                chunk_semantic(&content_owned, &lang_owned, threshold)
            }))
        })
        .await;

        let semantic_chunks = match chunk_result {
            Ok(Ok(chunks)) => chunks,
            Ok(Err(panic)) => {
                tracing::error!("RAG chunker panic for {source_path}: {panic:?}");
                return Ok(());
            }
            Err(join_err) => {
                tracing::error!("RAG chunker task failed for {source_path}: {join_err}");
                return Ok(());
            }
        };
        let embedding_client: Arc<dyn EmbeddingClient> = match client_from_config(&config) {
            Ok(client) => Arc::from(client),
            Err(error) => {
                tracing::error!(
                    "RAG: cannot index {source_path} — embedding client unavailable: {error}. \
                     Check that embeddings_model is configured and the API key env var is set."
                );
                return Ok(());
            }
        };

        let mut vector_chunks = Vec::with_capacity(semantic_chunks.len());
        for chunk in semantic_chunks {
            let content = chunk.content;
            let chunk_id = Uuid::new_v4().to_string();
            let client = Arc::clone(&embedding_client);
            let content_for_embedding = content.clone();
            let embedding =
                match tokio::task::spawn_blocking(move || client.embed(&content_for_embedding))
                    .await
                {
                    Ok(Ok(values)) => values,
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
                created_at: now,
            });
        }

        if vector_chunks.is_empty() {
            tracing::error!(
                "RAG: no chunks were embedded for {source_path} — file will not be indexed"
            );
            return Ok(());
        }

        sync_vector_store(&config, source_path, &vector_chunks).await;
        Ok(())
    }
}

async fn sync_vector_store(
    config: &crate::domain::canopy_config::CanopyConfig,
    source_path: &str,
    chunks: &[VectorChunk],
) {
    let Some(store) = open_vector_store(config).await else {
        return;
    };

    if let Err(error) = store.delete_by_path(source_path).await {
        tracing::warn!("RAG vector cleanup error {source_path}: {error}");
        return;
    }

    for chunk in chunks {
        if let Err(error) = store.insert_chunk(chunk).await {
            tracing::warn!(
                "RAG vector store error {source_path} chunk {}: {error}",
                chunk.id
            );
        }
    }
}

async fn purge_vector_chunks(data_dir: &Path, source_path: &str) {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);
    let Some(store) = open_vector_store(&config).await else {
        return;
    };

    if let Err(error) = store.delete_by_path(source_path).await {
        tracing::warn!("RAG vector cleanup error {source_path}: {error}");
    }
}

fn extract_file_content(path: &Path, lang: &str) -> anyhow::Result<String> {
    if lang == "text" && path.extension().and_then(|e| e.to_str()) == Some("pdf") {
        extract_pdf_text(path)
    } else {
        Ok(std::fs::read_to_string(path)?)
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
        Ok(dimensions) => dimensions,
        Err(error) => {
            tracing::warn!("RAG vector store unavailable for model {model}: {error}");
            return None;
        }
    };

    match VectorStore::new(dimensions).await {
        Ok(store) => Some(store),
        Err(error) => {
            tracing::warn!("RAG vector store open error: {error}");
            None
        }
    }
}
