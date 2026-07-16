#![allow(dead_code)]
//! `IngestionManager` — async queue + background worker for personal RAG indexing.
//!
//! Indexes only `.md`, `.mdx`, and `.pdf` files from the personal RAG root
//! (`~/.canopy/rag/` by default). Uses LanceDB as the vector search backend.

use std::collections::{HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use crate::application::ports::StateRepository;
use crate::db::Database;
use crate::rag::chunker::{chunk_semantic, detect_lang, SemanticChunk};
use crate::rag::embedding_client::{client_from_config, model_dimensions, EmbeddingClient};
use crate::rag::vector_store::{VectorChunk, VectorStore};

const QUEUE_MAX: usize = 10_000;
/// How often the background task checks whether the cached embedding client
/// has been idle long enough to unload.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Exposed so `doctor` and `rag report` can point at the same cap when
/// explaining why a file was skipped.
pub(crate) const FILE_MAX_BYTES: u64 = 5 * 1024 * 1024; // 5 MB
/// Max number of recorded `"error"` events before a file is given up on
/// permanently, even if the error message doesn't match a known-fatal pattern.
const MAX_RAG_ATTEMPTS: i64 = 3;

/// Returns true when an indexing error is non-transient (retrying will never help).
fn is_permanent_rag_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("overflowed its stack")
        || m.contains("stack overflow")
        || m.contains("invalidcontentstream")
        || m.contains("not a valid pdf")
        || m.contains("no extractable text")
}

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

/// A lazily-loaded embedding client plus enough bookkeeping to unload it
/// after it has sat idle, without racing an in-flight indexing operation.
struct CachedClient {
    model: String,
    client: Arc<dyn EmbeddingClient>,
    last_used: Instant,
}

pub struct IngestionManager {
    db: Arc<Database>,
    data_dir: PathBuf,
    queue: Arc<Mutex<Queue>>,
    notify: Arc<Notify>,
    _personal_watcher: std::sync::Mutex<Option<RecommendedWatcher>>,
    /// Cached embedding client keyed by model id so we load the ONNX model once.
    /// Dropped after `embeddings_idle_unload_secs` of inactivity (see
    /// `idle_unload_loop`) and reloaded transparently on next use.
    cached_client: Mutex<Option<CachedClient>>,
}

impl IngestionManager {
    pub fn new(db: Arc<Database>, data_dir: PathBuf) -> Self {
        crate::rag::ragignore::ensure_ragignore(&data_dir);
        // `cached_client` always starts empty, so the persisted flag other
        // processes read (see `rag::status`) must agree — otherwise a stale
        // "1" surviving a daemon restart would make `canopy rag report` lie
        // about the model being loaded before anything has queried it.
        let _ = db.set_state(crate::rag::status::RAG_MODEL_LOADED_KEY, "0");
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

    pub async fn refresh_snapshot(&self) {
        refresh_rag_snapshot(&self.db, &self.data_dir).await;
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
                        EventKind::Create(_) | EventKind::Modify(_)
                            if detect_lang(&path_str).is_some() =>
                        {
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

        let ct_run = ct.child_token();
        let mgr_run = Arc::clone(&self);
        tokio::spawn(async move {
            mgr_run.run(ct_run).await;
        });

        let ct_idle = ct.child_token();
        let mgr_idle = Arc::clone(&self);
        tokio::spawn(async move {
            mgr_idle.idle_unload_loop(ct_idle).await;
        });

        ct
    }

    /// Periodically checks whether the cached embedding client has been idle
    /// long enough to drop, freeing the model's RAM until it's needed again.
    async fn idle_unload_loop(&self, ct: tokio_util::sync::CancellationToken) {
        loop {
            tokio::select! {
                _ = ct.cancelled() => break,
                _ = tokio::time::sleep(IDLE_CHECK_INTERVAL) => {
                    let idle_timeout = std::time::Duration::from_secs(
                        crate::domain::canopy_config::CanopyConfig::load(&self.data_dir)
                            .embeddings_idle_unload_secs,
                    );
                    self.maybe_unload_idle_client(idle_timeout).await;
                }
            }
        }
    }

    /// Drops the cached embedding client if it has been idle for at least
    /// `idle_timeout` and nothing else currently holds a reference to it.
    async fn maybe_unload_idle_client(&self, idle_timeout: Duration) {
        if let Some((model, idle_for)) = self
            .maybe_unload_idle_client_at(idle_timeout, Instant::now())
            .await
        {
            tracing::info!(
                "RAG: unloaded embedding client for model '{model}' after {:.0}s idle",
                idle_for.as_secs_f64()
            );
        }
    }

    /// Testable core of `maybe_unload_idle_client`: takes `now` explicitly so
    /// tests can simulate an idle timeout without sleeping in real time.
    /// Never unloads while another clone of the `Arc` is still in flight
    /// (e.g. mid-indexing) — that's read straight off the strong count, not
    /// a separately-tracked "busy" flag, so it can't drift out of sync.
    /// Returns the unloaded model id plus how long it actually sat idle.
    async fn maybe_unload_idle_client_at(
        &self,
        idle_timeout: Duration,
        now: Instant,
    ) -> Option<(String, Duration)> {
        if idle_timeout.is_zero() {
            return None;
        }

        let mut guard = self.cached_client.lock().await;
        let cached = guard.as_ref()?;

        let idle_for = now.saturating_duration_since(cached.last_used);
        if idle_for < idle_timeout {
            return None;
        }
        if Arc::strong_count(&cached.client) > 1 {
            return None;
        }

        let model = cached.model.clone();
        *guard = None;
        let _ = self
            .db
            .set_state(crate::rag::status::RAG_MODEL_LOADED_KEY, "0");
        Some((model, idle_for))
    }

    /// Scan the vector store for chunks whose source file no longer exists on disk
    /// **or** falls outside the currently configured personal RAG roots, and delete
    /// them. Run once at startup to clean up orphans left from:
    ///  - files deleted while the daemon was offline, and
    ///  - a previous RAG root path that the user changed via `canopy setup`.
    pub async fn reconcile_orphan_chunks(&self) {
        let config = crate::domain::canopy_config::CanopyConfig::load(&self.data_dir);
        let Some(store) = open_vector_store(&config, &self.db).await else {
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

        // Build the set of currently configured roots (as PathBufs) so we can
        // check whether each indexed path still belongs to them.
        let personal_roots: Vec<std::path::PathBuf> = config
            .rag_personal_dirs
            .iter()
            .map(std::path::PathBuf::from)
            .collect();

        tracing::info!(
            "RAG reconcile: checking {} indexed path(s) for orphan chunks (roots: {:?})",
            paths.len(),
            personal_roots
        );
        let mut purged = 0usize;
        for path in &paths {
            let disk_path = std::path::Path::new(path);

            // Case 1: file no longer exists on disk.
            let missing_on_disk = !disk_path.exists();

            // Case 2: file exists but is outside every configured root
            // (i.e. the user changed the RAG root via `canopy setup`).
            let outside_roots = !personal_roots.is_empty()
                && !personal_roots
                    .iter()
                    .any(|root| disk_path.starts_with(root));

            if missing_on_disk || outside_roots {
                let reason = if missing_on_disk {
                    "file no longer on disk — orphan chunks purged at startup"
                } else {
                    "file outside configured RAG roots — purged after root change"
                };
                tracing::info!(
                    "RAG reconcile: '{}' — {} (missing={}, outside_roots={})",
                    path,
                    reason,
                    missing_on_disk,
                    outside_roots
                );
                if let Err(e) = store.delete_by_path(path).await {
                    tracing::warn!("RAG reconcile: failed to purge '{}': {e:#}", path);
                } else {
                    purged += 1;
                    let _ = self.db.log_rag_event(
                        path,
                        "deleted",
                        Some(reason),
                        chrono::Utc::now().timestamp(),
                    );
                }
            }
        }
        if purged > 0 {
            tracing::info!(
                "RAG reconcile: removed chunks for {purged} deleted/out-of-scope file(s)"
            );
            refresh_rag_snapshot(&self.db, &self.data_dir).await;
        } else {
            tracing::debug!("RAG reconcile: no orphan chunks found");
        }
    }

    /// Reconcile the ledger against the vector store at startup. If the
    /// ledger believes files are indexed but the store comes back empty —
    /// store loss, e.g. a hard crash destroyed the LanceDB directory while
    /// the SQLite ledger survived — purge the stale ledger rows and the
    /// pending queue so the caller's directory scan requeues everything for
    /// a full re-index. A healthy/consistent startup, or a store that failed
    /// to open, leaves everything untouched. Returns `true` if store loss
    /// was detected and handled.
    pub async fn reconcile_ledger_with_store(&self) -> bool {
        let store_chunk_count = self.store_chunk_count().await;
        let lancedb_path = match VectorStore::default_lancedb_path() {
            Ok(path) => path,
            Err(e) => {
                tracing::error!("RAG reconcile: cannot determine LanceDB path: {e:#}");
                return false;
            }
        };
        self.reconcile_ledger_with_store_chunk_count_at(store_chunk_count, &lancedb_path)
            .await
    }

    /// Core of `reconcile_ledger_with_store`, taking the store's chunk count
    /// and the LanceDB directory as plain values so the decision + purge
    /// logic can be unit tested against a scratch directory instead of the
    /// real home-dir-rooted vector store.
    async fn reconcile_ledger_with_store_chunk_count_at(
        &self,
        store_chunk_count: Option<i64>,
        lancedb_path: &Path,
    ) -> bool {
        let ledger_indexed_count = self.db.indexed_files_timestamps().unwrap_or_default().len();
        if !store_loss_detected(ledger_indexed_count, store_chunk_count) {
            return false;
        }

        tracing::warn!(
            "RAG reconcile: ledger reports {ledger_indexed_count} indexed file(s) but the \
             vector store has 0 chunks — the store was likely destroyed (e.g. a hard crash) \
             while the SQLite ledger survived; purging stale ledger rows and requeuing a full re-index"
        );
        if let Err(e) = wipe_lancedb_at(
            lancedb_path,
            &self.db,
            "ledger/store reconciliation: ledger has indexed files but store is empty",
        )
        .await
        {
            tracing::error!(
                "RAG reconcile: failed to purge ledger after detecting store loss: {e:#}"
            );
        }
        self.clear_queue().await;
        true
    }

    /// Chunk count in the vector store, or `None` if it could not be opened
    /// (no model configured, or a genuine — not transient — open failure).
    async fn store_chunk_count(&self) -> Option<i64> {
        let config = crate::domain::canopy_config::CanopyConfig::load(&self.data_dir);
        let store = open_vector_store(&config, &self.db).await?;
        match store.count_chunks().await {
            Ok(count) => Some(count),
            Err(e) => {
                tracing::warn!("RAG reconcile: failed to count vector store chunks: {e:#}");
                None
            }
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
                "RAG indexing",
                &format!("{initial_count} file(s) pending"),
                crate::domain::notification::NotificationLevel::Info,
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
            tracing::info!("RAG drain_queue: indexing '{}'", source_path);

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
                    let now = chrono::Utc::now().timestamp();
                    let _ = self
                        .db
                        .log_rag_event(&source_path, "error", Some(&error_detail), now);

                    let prior_errors = self.db.rag_error_count(&source_path).unwrap_or(0);
                    let attempt = prior_errors + 1;
                    let permanent =
                        is_permanent_rag_error(&error_detail) || attempt >= MAX_RAG_ATTEMPTS;

                    let filename = std::path::Path::new(&source_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| source_path.clone());

                    if permanent {
                        let _ =
                            self.db
                                .log_rag_event(&source_path, "failed", Some(&error_detail), now);
                        tracing::warn!(
                            "RAG index permanent failure {source_path}: giving up after {attempt} attempt(s)"
                        );
                        crate::domain::notification::send_notification(
                            "RAG indexing",
                            &format!(
                                "{filename} permanently failed after {attempt} attempt(s) — giving up\nCause: {error_detail}"
                            ),
                            crate::domain::notification::NotificationLevel::Error,
                        );
                    } else {
                        // Immediate per-file error notification so the user knows right away.
                        crate::domain::notification::send_notification(
                            "RAG indexing",
                            &format!("{filename} could not be indexed\nCause: {error_detail}"),
                            crate::domain::notification::NotificationLevel::Error,
                        );
                    }
                }
            }
        }

        if processed > 0 {
            let dir_note = indexing_dir_summary(&indexed_paths);
            tracing::info!("Personal RAG: indexed {processed} file(s){dir_note}");
            crate::domain::notification::send_notification(
                "RAG indexing",
                &format!("{processed} file(s) indexed{dir_note}"),
                crate::domain::notification::NotificationLevel::Success,
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
    /// This is the sole load point: the model is never loaded eagerly at
    /// startup, only here, on the first query or indexing pass that needs it.
    /// `pub(crate)` so `rag_search` shares the same cache (B22) instead of
    /// building throwaway clients that dodge the model-loaded status.
    pub(crate) async fn get_embedding_client(
        &self,
        config: &crate::domain::canopy_config::CanopyConfig,
    ) -> anyhow::Result<Arc<dyn EmbeddingClient>> {
        let model_id = config.embeddings_model.trim().to_string();
        let config_clone = config.clone();
        self.get_or_load_client(model_id, move || async move {
            // Load the model (potentially heavy for local ONNX models) off the async executor.
            tokio::task::spawn_blocking(move || client_from_config(&config_clone).map(Arc::from))
                .await
                .map_err(|e| anyhow::anyhow!("Embedding client task panicked: {e}"))?
        })
        .await
    }

    /// Cache-lookup-or-load core shared by `get_embedding_client` and tests: returns
    /// the cached client for `model_id` if present and refreshes its idle timer,
    /// otherwise runs `loader` and caches the result. Kept generic over `loader` so
    /// tests can exercise the caching/idle-tracking behavior with a lightweight mock
    /// client instead of a real (network- or ONNX-backed) `EmbeddingClient`.
    async fn get_or_load_client<F, Fut>(
        &self,
        model_id: String,
        loader: F,
    ) -> anyhow::Result<Arc<dyn EmbeddingClient>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Arc<dyn EmbeddingClient>>>,
    {
        let mut guard = self.cached_client.lock().await;

        if let Some(cached) = guard.as_mut() {
            if cached.model == model_id {
                cached.last_used = Instant::now();
                return Ok(Arc::clone(&cached.client));
            }
        }

        tracing::info!("RAG: loading embedding client for model '{model_id}'");
        let client = loader().await?;

        *guard = Some(CachedClient {
            model: model_id.clone(),
            client: Arc::clone(&client),
            last_used: Instant::now(),
        });
        tracing::info!("RAG: embedding client loaded and cached for model '{model_id}'");

        let _ = self
            .db
            .set_state(crate::rag::status::RAG_MODEL_LOADED_KEY, "1");
        let _ = self
            .db
            .set_state(crate::rag::status::RAG_MODEL_NAME_KEY, &model_id);
        let _ = self.db.set_state(
            crate::rag::status::RAG_MODEL_SINCE_KEY,
            &chrono::Utc::now().timestamp().to_string(),
        );

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
            let size_mb = meta.len() as f64 / (1024.0 * 1024.0);
            let cap_mb = FILE_MAX_BYTES as f64 / (1024.0 * 1024.0);
            tracing::info!(
                "Personal RAG: skipping '{source_path}' — {size_mb:.1} MB exceeds the \
                 {cap_mb:.0} MB indexing limit (FILE_MAX_BYTES)"
            );
            record_oversize_skip(&self.db, source_path, meta.len());
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
        sync_vector_store(
            &self.db,
            &self.data_dir,
            &config,
            source_path,
            &vector_chunks,
        )
        .await?;
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
    db: &Database,
    data_dir: &Path,
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

    let Some(store) = open_vector_store(config, db).await else {
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
    if ok > 0 {
        refresh_rag_snapshot(db, data_dir).await;
    }
    Ok(())
}

async fn purge_vector_chunks(data_dir: &Path, source_path: &str, db: Option<&crate::db::Database>) {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);
    let Some(store) = open_vector_store(&config, db.unwrap()).await else {
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
                refresh_rag_snapshot(db, data_dir).await;
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

/// Record a `"skipped_oversize"` ledger event for a file that exceeds
/// `FILE_MAX_BYTES`, unless the latest recorded event for that path is
/// already a `"skipped_oversize"` for the same size — the file is rescanned
/// on every startup and watcher pass, so without this check an unchanged
/// oversize file would spam a duplicate row every time.
fn record_oversize_skip(db: &Database, source_path: &str, size_bytes: u64) {
    let detail = size_bytes.to_string();
    let already_recorded = db
        .rag_events_for_file(source_path)
        .ok()
        .and_then(|events| events.into_iter().next())
        .is_some_and(|e| {
            e.event_type == "skipped_oversize" && e.detail.as_deref() == Some(detail.as_str())
        });
    if already_recorded {
        return;
    }
    let _ = db.log_rag_event(
        source_path,
        "skipped_oversize",
        Some(&detail),
        chrono::Utc::now().timestamp(),
    );
}

async fn refresh_rag_snapshot(db: &Database, data_dir: &Path) {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);
    let Some(store) = open_vector_store(&config, db).await else {
        let _ = db.set_state("rag_total_chunks", "0");
        let _ = db.set_state("rag_indexed_files", "0");
        return;
    };

    let total_chunks = store.count_chunks().await.unwrap_or(0);
    let indexed_files = store.count_unique_paths().await.unwrap_or(0);
    let _ = db.set_state("rag_total_chunks", &total_chunks.to_string());
    let _ = db.set_state("rag_indexed_files", &indexed_files.to_string());
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

/// Internal helper command used to isolate PDF parsing in a subprocess.
/// This prevents parser-level stack overflows from taking down the daemon.
pub fn run_internal_pdf_extract(path: &Path) -> anyhow::Result<()> {
    let text = extract_pdf_text_in_process(path)?;
    print!("{text}");
    Ok(())
}

fn extract_pdf_text(path: &Path) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let exe_display = exe.display().to_string();
    let output = std::process::Command::new(&exe)
        .arg("internal-pdf-extract")
        .arg(path)
        .output()
        .map_err(|e| {
            anyhow::anyhow!("Failed to launch PDF extraction subprocess ({exe_display}): {e}")
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("subprocess exited with status {}", output.status)
        } else {
            stderr
        };
        anyhow::bail!(
            "PDF extraction subprocess failed for {}: {detail}",
            path.display()
        );
    }

    String::from_utf8(output.stdout).map_err(|e| {
        anyhow::anyhow!(
            "PDF extraction subprocess returned non-UTF8 output for {}: {e}",
            path.display()
        )
    })
}

fn extract_pdf_text_in_process(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    // Detect actual file type by magic bytes — .pdf files are sometimes HTML
    // error pages or other content saved with the wrong extension.
    if is_html_bytes(&buffer) {
        let html = String::from_utf8_lossy(&buffer);
        let text = strip_html_to_text(&html);
        if text.trim().is_empty() {
            anyhow::bail!(
                "File '{}' appears to be HTML but contains no extractable text",
                path.display()
            );
        }
        tracing::info!(
            "RAG: '{}' detected as HTML (not PDF) — extracted {} bytes via HTML stripper",
            path.display(),
            text.len()
        );
        return Ok(text);
    }

    if !buffer.starts_with(b"%PDF") {
        // Unknown binary — attempt pdf-extract anyway, fall back to raw salvage.
        tracing::warn!(
            "RAG: '{}' missing PDF magic bytes — attempting pdf-extract with raw-text fallback",
            path.display()
        );
        return pdf_extract::extract_text_from_mem(&buffer)
            .map_err(|_| ())
            .or_else(|_| {
                let text = salvage_printable_text(&buffer);
                if text.split_whitespace().count() >= 20 {
                    Ok(text)
                } else {
                    Err(())
                }
            })
            .map_err(|_| {
                anyhow::anyhow!(
                    "Could not extract text from '{}': not a valid PDF or recognisable text file",
                    path.display()
                )
            });
    }

    pdf_extract::extract_text_from_mem(&buffer)
        .map_err(|e| anyhow::anyhow!("PDF text extraction failed for {}: {e}", path.display()))
}

/// Returns `true` when the byte slice looks like an HTML document.
fn is_html_bytes(bytes: &[u8]) -> bool {
    // Skip a leading UTF-8 BOM if present.
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let prefix = &bytes[..bytes.len().min(64)];
    let lower: Vec<u8> = prefix.iter().map(|b| b.to_ascii_lowercase()).collect();
    lower.starts_with(b"<!doctype")
        || lower.starts_with(b"<html")
        || lower.windows(6).any(|w| w == b"<html ")
}

/// Very simple HTML-to-text: strips tags, decodes common entities, collapses whitespace.
fn strip_html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut state = HtmlStripState::new();
    let mut chars = html.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '&' && !state.in_tag && !state.in_script {
            decode_html_entity(&mut chars, &mut out);
            state.pending_space = false;
        } else {
            state.process_char(ch, &mut out);
        }
    }

    collapse_blank_lines(&out)
}

struct HtmlStripState {
    in_tag: bool,
    in_script: bool,
    tag_buf: String,
    pending_space: bool,
}

impl HtmlStripState {
    fn new() -> Self {
        Self {
            in_tag: false,
            in_script: false,
            tag_buf: String::new(),
            pending_space: false,
        }
    }

    fn process_char(&mut self, ch: char, out: &mut String) {
        if self.in_tag {
            self.handle_tag_char(ch, out);
        } else if ch == '<' {
            self.in_tag = true;
            self.tag_buf.clear();
            self.tag_buf.push(ch);
        } else if self.in_script {
            // skip script/style content
        } else if ch.is_whitespace() {
            self.pending_space = true;
        } else {
            self.flush_pending_space(out);
            out.push(ch);
        }
    }

    fn handle_tag_char(&mut self, ch: char, out: &mut String) {
        self.tag_buf.push(ch);
        if ch != '>' {
            return;
        }

        let tag_name = extract_tag_name(&self.tag_buf);
        self.in_script = matches!(tag_name.as_str(), "script" | "style");

        if is_block_level_tag(&tag_name) {
            out.push('\n');
            self.pending_space = false;
        }

        self.tag_buf.clear();
        self.in_tag = false;
    }

    fn flush_pending_space(&mut self, out: &mut String) {
        if self.pending_space && !out.ends_with('\n') {
            out.push(' ');
        }
        self.pending_space = false;
    }
}

fn decode_html_entity(chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    let mut entity = String::new();
    for ec in chars.by_ref() {
        if ec == ';' {
            break;
        }
        entity.push(ec);
        if entity.len() > 8 {
            break;
        }
    }
    out.push_str(decode_entity(&entity));
}

fn decode_entity(entity: &str) -> &'static str {
    match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "nbsp" | "#160" => " ",
        "quot" => "\"",
        "apos" | "#39" => "'",
        _ => " ",
    }
}

fn extract_tag_name(tag_buf: &str) -> String {
    let tag_lower = tag_buf.to_ascii_lowercase();
    tag_lower
        .trim_start_matches('<')
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '>')
        .next()
        .unwrap_or("")
        .to_owned()
}

fn is_block_level_tag(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "p" | "div"
            | "br"
            | "li"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "tr"
            | "td"
            | "th"
            | "blockquote"
            | "section"
            | "article"
    )
}

fn collapse_blank_lines(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut blank_run = 0usize;
    for line in input.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                result.push('\n');
            }
        } else {
            blank_run = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

/// Last-resort text salvage: collect printable ASCII / Unicode runs from raw bytes.
/// Only accepts runs of at least 4 consecutive printable chars to filter binary noise.
fn salvage_printable_text(bytes: &[u8]) -> String {
    const MIN_RUN: usize = 4;
    let mut runs: Vec<String> = Vec::new();
    let mut current = String::new();

    for &b in bytes {
        if (0x20..0x7f).contains(&b) || b == b'\n' || b == b'\r' || b == b'\t' {
            current.push(b as char);
        } else {
            if current.trim().len() >= MIN_RUN {
                runs.push(current.trim().to_owned());
            }
            current.clear();
        }
    }
    if current.trim().len() >= MIN_RUN {
        runs.push(current.trim().to_owned());
    }

    runs.join(" ")
}

/// Gate: at most one corruption-driven purge per process lifetime.
static LANCEDB_PURGE_DONE: AtomicBool = AtomicBool::new(false);

async fn open_vector_store(
    config: &crate::domain::canopy_config::CanopyConfig,
    db: &Database,
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
            // If the store genuinely fails to open AND we haven't purged yet this
            // process, remove the corrupted directory and retry exactly once.
            // A missing directory is NOT an error — VectorStore::new creates it
            // fresh — so any Err means the store is genuinely corrupted.
            if !LANCEDB_PURGE_DONE.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "RAG vector store is corrupted ({error:#}); \
                     purging directory and retrying once"
                );
                if let Err(e) = wipe_lancedb(db, "corrupt store recovery").await {
                    tracing::warn!("RAG: failed to purge corrupt store: {e:#}");
                }
                match VectorStore::new(dimensions).await {
                    Ok(store) => {
                        tracing::warn!("RAG open_vector_store: recovered after purge");
                        return Some(store);
                    }
                    Err(retry_err) => {
                        tracing::warn!(
                            "RAG open_vector_store: retry after purge also failed: {retry_err:#}"
                        );
                    }
                }
            } else {
                tracing::warn!("RAG vector store open error (purge already used): {error:#}");
            }
            None
        }
    }
}

/// Decide whether the vector store has silently lost data relative to the
/// ledger — the signature of a hard crash that destroyed the LanceDB
/// directory while the SQLite ledger (`rag_file_events`) survived. A store
/// that failed to open (`None`) is never treated as loss: per the A4 fix, a
/// transient open error on a known-existing table must not be conflated with
/// "the store is empty".
fn store_loss_detected(ledger_indexed_count: usize, store_chunk_count: Option<i64>) -> bool {
    ledger_indexed_count > 0 && store_chunk_count == Some(0)
}

/// Delete the entire LanceDB directory so the next `open_vector_store` starts fresh.
/// Used when the embeddings model is changed so stale vectors don't pollute results.
pub async fn wipe_lancedb(db: &Database, reason: &str) -> anyhow::Result<()> {
    let lancedb_path = VectorStore::default_lancedb_path()?;
    wipe_lancedb_at(&lancedb_path, db, reason).await
}

/// Core of `wipe_lancedb`, taking the LanceDB directory as a plain path so
/// tests can point it at a scratch directory instead of the real
/// home-directory-rooted store.
pub(crate) async fn wipe_lancedb_at(
    lancedb_path: &Path,
    db: &Database,
    reason: &str,
) -> anyhow::Result<()> {
    if lancedb_path.exists() {
        tokio::fs::remove_dir_all(lancedb_path).await.map_err(|e| {
            anyhow::anyhow!("Failed to wipe LanceDB at {}: {e}", lancedb_path.display())
        })?;
        tracing::info!(
            "RAG: wiped LanceDB at {} ({reason})",
            lancedb_path.display()
        );
    }
    // Clear rag_file_events so the report starts clean for the new model.
    let _ = db.clear_rag_file_events();
    let _ = db.set_state("rag_total_chunks", "0");
    let _ = db.set_state("rag_indexed_files", "0");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_permanent_rag_error_detects_known_fatal_patterns() {
        assert!(is_permanent_rag_error(
            "thread 'main' has overflowed its stack"
        ));
        assert!(is_permanent_rag_error("Stack overflow detected"));
        assert!(is_permanent_rag_error(
            "PDF text extraction failed for foo.pdf: InvalidContentStream"
        ));
        assert!(is_permanent_rag_error(
            "Could not extract text from 'foo.pdf': not a valid PDF or recognisable text file"
        ));
        assert!(is_permanent_rag_error(
            "File 'foo.html' appears to be HTML but contains no extractable text"
        ));
    }

    #[test]
    fn is_permanent_rag_error_allows_transient_messages() {
        assert!(!is_permanent_rag_error("connection reset by peer"));
        assert!(!is_permanent_rag_error("embedding request timed out"));
        assert!(!is_permanent_rag_error(
            "Failed to launch PDF extraction subprocess (/usr/bin/canopy): No such file or directory"
        ));
    }

    /// Mirrors the permanence decision made in `drain_queue`'s `Err` branch:
    /// a file is given up on once it has accumulated `MAX_RAG_ATTEMPTS`
    /// total attempts (prior errors + the current one), even for otherwise
    /// generic/transient-looking error messages.
    fn is_permanent(error_detail: &str, prior_errors: i64) -> bool {
        let attempt = prior_errors + 1;
        is_permanent_rag_error(error_detail) || attempt >= MAX_RAG_ATTEMPTS
    }

    #[test]
    fn permanence_decision_respects_attempt_threshold() {
        let transient = "embedding request timed out";
        assert!(!is_permanent(transient, 0)); // attempt 1
        assert!(!is_permanent(transient, 1)); // attempt 2
        assert!(is_permanent(transient, 2)); // attempt 3 == MAX_RAG_ATTEMPTS
        assert!(is_permanent(transient, 5)); // well past the threshold
    }

    #[test]
    fn permanence_decision_short_circuits_on_fatal_message() {
        // A known-fatal message is permanent immediately, regardless of attempt count.
        assert!(is_permanent("document overflowed its stack", 0));
    }

    #[test]
    fn lancedb_purge_flag_prevents_double_purge() {
        // Reset the flag so this test is deterministic.
        LANCEDB_PURGE_DONE.store(false, Ordering::SeqCst);

        // First swap should return false (not yet purged) and set the flag.
        assert!(
            !LANCEDB_PURGE_DONE.swap(true, Ordering::SeqCst),
            "first purge check should return false"
        );

        // Second swap should return true (already purged) — no second purge.
        assert!(
            LANCEDB_PURGE_DONE.swap(true, Ordering::SeqCst),
            "second purge check should return true (already purged)"
        );

        // Reset for other tests.
        LANCEDB_PURGE_DONE.store(false, Ordering::SeqCst);
    }

    use crate::rag::embedding_client::MockEmbeddingClient;

    fn test_manager() -> (IngestionManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        let mgr = IngestionManager::new(db, dir.path().to_path_buf());
        (mgr, dir)
    }

    fn mock_client() -> Arc<dyn EmbeddingClient> {
        Arc::new(MockEmbeddingClient::new(4))
    }

    /// (1) After startup with an empty queue, the embedding client is not loaded.
    #[tokio::test]
    async fn embedding_client_not_loaded_on_startup() {
        let (mgr, _dir) = test_manager();
        assert!(mgr.cached_client.lock().await.is_none());
    }

    /// (2) A query (via `get_or_load_client`, the shared core behind
    /// `get_embedding_client`) loads and caches the client on first use.
    #[tokio::test]
    async fn query_loads_embedding_client() {
        let (mgr, _dir) = test_manager();
        assert!(mgr.cached_client.lock().await.is_none());

        let client = mgr
            .get_or_load_client("mock-model".to_string(), || async { Ok(mock_client()) })
            .await
            .expect("load should succeed");
        assert_eq!(client.embed("hello").unwrap().len(), 4);

        let guard = mgr.cached_client.lock().await;
        let cached = guard.as_ref().expect("client should now be cached");
        assert_eq!(cached.model, "mock-model");
        drop(guard);

        assert!(crate::rag::status::is_model_loaded(&mgr.db));
        assert!(crate::rag::status::model_loaded_since(&mgr.db).is_some());
    }

    /// A fresh manager persists "not loaded" so a stale flag left over from a
    /// previous daemon run (the DB file survives restarts) can't make
    /// `canopy rag report` claim the model is warm before anything has used it.
    #[tokio::test]
    async fn new_manager_persists_model_not_loaded() {
        let (mgr, _dir) = test_manager();
        assert!(!crate::rag::status::is_model_loaded(&mgr.db));
    }

    /// (3) Simulated idle timeout releases the cached client.
    #[tokio::test]
    async fn idle_timeout_unloads_cached_client() {
        let (mgr, _dir) = test_manager();
        mgr.get_or_load_client("mock-model".to_string(), || async { Ok(mock_client()) })
            .await
            .unwrap();
        assert!(mgr.cached_client.lock().await.is_some());

        let idle_timeout = Duration::from_secs(600);
        let long_after = Instant::now() + Duration::from_secs(700);
        let unloaded = mgr
            .maybe_unload_idle_client_at(idle_timeout, long_after)
            .await;

        assert_eq!(
            unloaded.map(|(model, _)| model),
            Some("mock-model".to_string())
        );
        assert!(mgr.cached_client.lock().await.is_none());
        assert!(!crate::rag::status::is_model_loaded(&mgr.db));
    }

    /// (4) The client is not released while an in-progress use still holds the Arc,
    /// even past the idle timeout — checked via the strong count, not a busy flag.
    #[tokio::test]
    async fn idle_unload_skipped_while_client_in_use() {
        let (mgr, _dir) = test_manager();
        // Simulates an in-flight indexing/query call still holding the Arc it
        // got back from `get_or_load_client` (the cache itself holds a second
        // strong reference, so the count is 2 while `held` is alive).
        let held = mgr
            .get_or_load_client("mock-model".to_string(), || async { Ok(mock_client()) })
            .await
            .unwrap();

        let idle_timeout = Duration::from_secs(600);
        let long_after = Instant::now() + Duration::from_secs(700);
        let unloaded = mgr
            .maybe_unload_idle_client_at(idle_timeout, long_after)
            .await;

        assert!(
            unloaded.is_none(),
            "must not unload while a use is in flight"
        );
        assert!(mgr.cached_client.lock().await.is_some());

        drop(held);
        let unloaded = mgr
            .maybe_unload_idle_client_at(idle_timeout, long_after)
            .await;
        assert!(
            unloaded.is_some(),
            "should unload once the in-flight use ends"
        );
        assert!(mgr.cached_client.lock().await.is_none());
    }

    #[test]
    fn store_loss_detected_when_ledger_has_files_but_store_is_empty() {
        assert!(store_loss_detected(310, Some(0)));
    }

    #[test]
    fn store_loss_not_detected_when_ledger_and_store_agree() {
        // Consistent state: ledger has entries and the store has chunks too.
        assert!(!store_loss_detected(310, Some(1200)));
    }

    #[test]
    fn store_loss_not_detected_when_ledger_is_empty() {
        // Nothing indexed yet — an empty store is expected, not a loss.
        assert!(!store_loss_detected(0, Some(0)));
    }

    #[test]
    fn store_loss_not_detected_on_transient_open_failure() {
        // A4 protection: a store that failed to open (`None`) must never be
        // conflated with "the store is empty" — that would wipe a healthy
        // ledger on a transient error.
        assert!(!store_loss_detected(310, None));
    }

    /// Ledger reports indexed files but the store comes back with zero
    /// chunks (store loss) → the ledger and pending queue are purged so the
    /// caller's directory scan requeues everything for a full re-index.
    #[tokio::test]
    async fn reconcile_purges_ledger_and_queue_when_store_loss_detected() {
        let (mgr, _dir) = test_manager();
        mgr.db()
            .log_rag_event(
                "/docs/a.md",
                "indexed",
                None,
                chrono::Utc::now().timestamp(),
            )
            .unwrap();
        mgr.db()
            .log_rag_event(
                "/docs/b.md",
                "indexed",
                None,
                chrono::Utc::now().timestamp(),
            )
            .unwrap();
        mgr.enqueue("/docs/stale.md").await;
        assert_eq!(mgr.db().indexed_files_timestamps().unwrap().len(), 2);

        // A scratch path standing in for the (destroyed/empty) LanceDB
        // directory — never the real home-dir-rooted store.
        let scratch_lancedb = tempfile::tempdir().unwrap();
        let handled = mgr
            .reconcile_ledger_with_store_chunk_count_at(Some(0), scratch_lancedb.path())
            .await;

        assert!(handled, "store loss should have been detected and handled");
        assert!(mgr.db().indexed_files_timestamps().unwrap().is_empty());
        assert!(mgr.db_pending_queue().unwrap().is_empty());
        assert_eq!(mgr.queue_len().await, 0);
    }

    /// Ledger and store agree (store has chunks) → nothing is purged and
    /// nothing extra is queued.
    #[tokio::test]
    async fn reconcile_leaves_healthy_state_untouched() {
        let (mgr, _dir) = test_manager();
        mgr.db()
            .log_rag_event(
                "/docs/a.md",
                "indexed",
                None,
                chrono::Utc::now().timestamp(),
            )
            .unwrap();
        mgr.enqueue("/docs/pending.md").await;

        let scratch_lancedb = tempfile::tempdir().unwrap();
        let handled = mgr
            .reconcile_ledger_with_store_chunk_count_at(Some(5), scratch_lancedb.path())
            .await;

        assert!(!handled, "consistent state must not trigger reconciliation");
        assert_eq!(mgr.db().indexed_files_timestamps().unwrap().len(), 1);
        assert_eq!(mgr.db_pending_queue().unwrap(), vec!["/docs/pending.md"]);
        assert_eq!(mgr.queue_len().await, 1);
    }

    /// A file over `FILE_MAX_BYTES` records a `"skipped_oversize"` ledger
    /// event carrying its size, instead of vanishing silently.
    #[tokio::test]
    async fn record_oversize_skip_logs_event_with_size() {
        let (mgr, _dir) = test_manager();
        record_oversize_skip(mgr.db(), "/docs/huge.pdf", 6_000_000);

        let events = mgr.db().rag_events_for_file("/docs/huge.pdf").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "skipped_oversize");
        assert_eq!(events[0].detail.as_deref(), Some("6000000"));
    }

    /// Recording the same oversize skip again (e.g. on the next startup scan,
    /// file unchanged) must not duplicate the ledger row.
    #[tokio::test]
    async fn record_oversize_skip_is_idempotent_for_unchanged_size() {
        let (mgr, _dir) = test_manager();
        record_oversize_skip(mgr.db(), "/docs/huge.pdf", 6_000_000);
        record_oversize_skip(mgr.db(), "/docs/huge.pdf", 6_000_000);
        record_oversize_skip(mgr.db(), "/docs/huge.pdf", 6_000_000);

        let events = mgr.db().rag_events_for_file("/docs/huge.pdf").unwrap();
        assert_eq!(
            events.len(),
            1,
            "unchanged oversize file must not spam events"
        );
    }

    /// If the file's size changes (e.g. grew further), a new event is
    /// recorded so the ledger reflects the current size.
    #[tokio::test]
    async fn record_oversize_skip_records_new_event_when_size_changes() {
        let (mgr, _dir) = test_manager();
        record_oversize_skip(mgr.db(), "/docs/huge.pdf", 6_000_000);
        record_oversize_skip(mgr.db(), "/docs/huge.pdf", 7_000_000);

        let events = mgr.db().rag_events_for_file("/docs/huge.pdf").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].detail.as_deref(), Some("7000000"));
    }

    /// End-to-end: `index_file` on a file above the cap does not error, does
    /// not index anything, and leaves a `"skipped_oversize"` ledger trail.
    #[tokio::test]
    async fn index_file_skips_oversize_file_and_records_ledger_event() {
        let (mgr, dir) = test_manager();
        let big_path = dir.path().join("huge.md");
        std::fs::write(&big_path, vec![b'a'; (FILE_MAX_BYTES + 1) as usize]).unwrap();
        let source_path = big_path.to_string_lossy().to_string();

        mgr.index_file(&source_path).await.unwrap();

        let events = mgr.db().rag_events_for_file(&source_path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "skipped_oversize");
        assert_eq!(
            events[0].detail.as_deref(),
            Some((FILE_MAX_BYTES + 1).to_string().as_str())
        );

        // Re-running against the unchanged file must not duplicate the event.
        mgr.index_file(&source_path).await.unwrap();
        let events = mgr.db().rag_events_for_file(&source_path).unwrap();
        assert_eq!(events.len(), 1);
    }
}
