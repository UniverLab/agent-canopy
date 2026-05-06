#![allow(dead_code)]
//! `IngestionManager` — async queue + background worker for personal RAG indexing.
//!
//! Indexes only `.md`, `.mdx`, and `.pdf` files from the personal RAG root
//! (`~/.canopy/rag/` by default).  Uses SQLite FTS5 as the search backend.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use crate::application::ports::StateRepository;
use crate::db::project::Chunk;
use crate::db::Database;
use crate::rag::chunker::{chunk_semantic, detect_lang};
use crate::rag::embedding_client::client_from_config;

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

    /// Push a path to the back of the queue.  If already present, move to end.
    /// Returns `false` when the queue is at capacity.
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
    /// Holds the personal-RAG notify watcher so it is not dropped.
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

    /// Enqueue a file path for (re)indexing.  Returns `false` if the queue is full.
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

    /// Queue size snapshot.
    pub async fn queue_len(&self) -> usize {
        self.queue.lock().await.len()
    }

    /// Return paths of items currently queued in the DB (from a previous session).
    pub fn db_pending_queue(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .db
            .list_rag_queue(10_000)?
            .into_iter()
            .map(|i| i.source_path)
            .collect())
    }

    /// Start recursive filesystem watchers on all `personal_roots`.
    ///
    /// - Create/Modify events enqueue the file for (re)indexing (if supported).
    /// - Delete events remove the file's chunks from the database immediately.
    ///
    /// All watchers are kept alive inside `self` for the lifetime of the manager.
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
                    // Find the matching root for this path to compute relative ignore check.
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
                            let db2 = Arc::clone(&db);
                            rt.spawn(async move {
                                let _ = db2.replace_chunks(&path_str, &[]);
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

    /// Start the background indexing worker.  Returns a cancellation token.
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
                    // Debounce: wait 3 s for burst to settle
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

    /// Spin-wait while RAG is paused.  Returns `false` if cancelled.
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
            self.db.replace_chunks(source_path, &[])?;
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

        let content = std::fs::read_to_string(path)?;
        let now = chrono::Utc::now().timestamp();
        let config = crate::domain::canopy_config::CanopyConfig::load(&self.data_dir);
        let semantic_chunks = chunk_semantic(&content, lang, config.similarity_threshold);
        let embedding_client = match client_from_config(&config) {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!("RAG embeddings unavailable for {source_path}: {error}");
                None
            }
        };

        let mut chunks = Vec::with_capacity(semantic_chunks.len());
        for chunk in semantic_chunks {
            let embedding = if let Some(client) = embedding_client.as_ref() {
                match client.embed(&chunk.content) {
                    Ok(values) => Some(values),
                    Err(error) => {
                        tracing::warn!(
                            "RAG embedding error {source_path} chunk {}: {error}",
                            chunk.index
                        );
                        None
                    }
                }
            } else {
                None
            };

            chunks.push(Chunk {
                id: Uuid::new_v4().to_string(),
                project_hash: None,
                source_path: source_path.to_owned(),
                chunk_index: chunk.index as i32,
                content: chunk.content,
                lang: lang.to_owned(),
                embedding,
                updated_at: now,
            });
        }

        self.db.replace_chunks(source_path, &chunks)?;
        Ok(())
    }
}
