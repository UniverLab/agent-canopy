//! Truthful RAG embedding-model status, shared by `canopy rag report` and the TUI.
//!
//! The embedding model loads lazily and unloads after idle (see
//! `rag::ingestion::IngestionManager`'s `cached_client`). The daemon persists
//! whether that cache currently holds a loaded model into `daemon_state` —
//! the same key/value store the CLI and TUI already read for `rag_paused`,
//! queue counts, etc. — so those separate processes can observe it without
//! reaching into the daemon's in-memory state (and without triggering a load
//! themselves, since this is a passive read).

use crate::application::ports::StateRepository;
use crate::db::Database;

/// Whether the cached embedding client currently holds a loaded model ("1"/"0").
pub(crate) const RAG_MODEL_LOADED_KEY: &str = "rag_model_loaded";
/// Model id last loaded into the cache (set alongside `RAG_MODEL_LOADED_KEY`).
pub(crate) const RAG_MODEL_NAME_KEY: &str = "rag_model_name";
/// Unix timestamp of when the currently-cached model finished loading.
pub(crate) const RAG_MODEL_SINCE_KEY: &str = "rag_model_loaded_since";

/// A local embedding model's acquisition (download + first-load) state,
/// persisted in `daemon_state` under a per-model key so it's visible across
/// processes (the CLI, the TUI, and the daemon's own background acquisition
/// loop all read the same rows) and survives a daemon restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquisitionState {
    /// Fetching the model's files from HuggingFace Hub. `started_at` is when
    /// acquisition began (not just this phase), so elapsed-time reporting
    /// spans the whole download+prepare, not one leg of it.
    Downloading { started_at: i64 },
    /// Files are on disk; building the ONNX session (the work
    /// `fastembed::TextEmbedding::try_new` does after its own download step).
    Preparing { started_at: i64 },
    /// The last acquisition attempt failed. Persists until explicitly
    /// retried (`canopy rag model retry`) — never auto-retried, so a bad
    /// network or a bad model id doesn't hammer HuggingFace forever.
    Failed { reason: String },
}

const ACQUISITION_PHASE_SUFFIX: &str = "phase";
const ACQUISITION_STARTED_AT_SUFFIX: &str = "started_at";
const ACQUISITION_ERROR_SUFFIX: &str = "error";

fn acquisition_key(model: &str, suffix: &str) -> String {
    format!("rag_download:{model}:{suffix}")
}

/// Read the persisted acquisition state for `model`, if any is currently
/// tracked. `None` means the model is either already fully available or
/// acquisition was never attempted — the normal ready/sleeping logic applies.
/// A passive lookup, like `is_model_loaded` — it never triggers anything.
pub fn read_acquisition_state(db: &Database, model: &str) -> Option<AcquisitionState> {
    let phase = db
        .get_state(&acquisition_key(model, ACQUISITION_PHASE_SUFFIX))
        .ok()
        .flatten()?;
    let started_at = || {
        db.get_state(&acquisition_key(model, ACQUISITION_STARTED_AT_SUFFIX))
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    match phase.as_str() {
        "downloading" => Some(AcquisitionState::Downloading {
            started_at: started_at(),
        }),
        "preparing" => Some(AcquisitionState::Preparing {
            started_at: started_at(),
        }),
        "failed" => {
            let reason = db
                .get_state(&acquisition_key(model, ACQUISITION_ERROR_SUFFIX))
                .ok()
                .flatten()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown error".to_string());
            Some(AcquisitionState::Failed { reason })
        }
        _ => None,
    }
}

/// Marks `model` as actively downloading. Called once, before any network
/// request, so a concurrent reader (another process, or a query racing the
/// background acquisition loop) sees it immediately rather than blocking.
/// Idempotent w.r.t. `started_at`: calling it twice for the same model (e.g.
/// once to claim the download before any lock is taken, once more from
/// inside the acquisition itself) preserves the earlier timestamp rather
/// than resetting the elapsed-time clock.
///
/// Only ever called from `local-embeddings`-gated code (nothing downloads
/// anything without that feature) — cfg-gated to match, so it isn't dead
/// code in a build without it.
#[cfg(feature = "local-embeddings")]
pub fn mark_downloading(db: &Database, model: &str) {
    let _ = db.set_state(
        &acquisition_key(model, ACQUISITION_PHASE_SUFFIX),
        "downloading",
    );
    let started_key = acquisition_key(model, ACQUISITION_STARTED_AT_SUFFIX);
    if db.get_state(&started_key).ok().flatten().is_none() {
        let now = chrono::Utc::now().timestamp();
        let _ = db.set_state(&started_key, &now.to_string());
    }
}

/// Transitions to the "preparing" phase (files on disk, building the ONNX
/// session). Leaves `started_at` alone if already set by `mark_downloading`
/// — but sets it here too for the already-cached-but-never-tracked case, so
/// elapsed-time reporting is still meaningful.
#[cfg(feature = "local-embeddings")]
pub fn mark_preparing(db: &Database, model: &str) {
    let _ = db.set_state(
        &acquisition_key(model, ACQUISITION_PHASE_SUFFIX),
        "preparing",
    );
    let started_key = acquisition_key(model, ACQUISITION_STARTED_AT_SUFFIX);
    if db.get_state(&started_key).ok().flatten().is_none() {
        let now = chrono::Utc::now().timestamp();
        let _ = db.set_state(&started_key, &now.to_string());
    }
}

/// Marks `model`'s acquisition as failed, with `reason` persisted for
/// display. Left in place (not auto-cleared) until `clear_acquisition` is
/// called by the retry command — the background acquisition loop checks for
/// this and does not automatically retry.
#[cfg(feature = "local-embeddings")]
pub fn mark_failed(db: &Database, model: &str, reason: &str) {
    let _ = db.set_state(&acquisition_key(model, ACQUISITION_PHASE_SUFFIX), "failed");
    let _ = db.set_state(&acquisition_key(model, ACQUISITION_ERROR_SUFFIX), reason);
}

/// Clears all acquisition tracking for `model` — called on success (nothing
/// left to report) and by the retry command (so the background loop's next
/// tick treats the model as eligible for a fresh attempt).
pub fn clear_acquisition(db: &Database, model: &str) {
    let _ = db.set_state(&acquisition_key(model, ACQUISITION_PHASE_SUFFIX), "");
    let _ = db.set_state(&acquisition_key(model, ACQUISITION_ERROR_SUFFIX), "");
}

/// Human-readable explanation of an in-progress or failed acquisition, meant
/// for a query that lands while the model isn't ready — names the state and,
/// for an in-progress download, how long it's been running (fastembed's
/// public API exposes no byte-level progress callback, only a bool that
/// toggles its own internal terminal progress bar, so elapsed time is the
/// most honest "remaining work" signal available here).
pub fn acquisition_message(model: &str, state: &AcquisitionState) -> String {
    match state {
        AcquisitionState::Downloading { started_at } => format!(
            "Embedding model '{model}' is still downloading ({}s so far). Try again shortly.",
            elapsed_secs(*started_at)
        ),
        AcquisitionState::Preparing { started_at } => format!(
            "Embedding model '{model}' finished downloading and is being prepared ({}s so far). \
             Try again shortly.",
            elapsed_secs(*started_at)
        ),
        AcquisitionState::Failed { reason } => format!(
            "Embedding model '{model}' failed to download: {reason}. \
             Run 'canopy rag model retry' to try again."
        ),
    }
}

pub fn elapsed_secs(started_at: i64) -> i64 {
    (chrono::Utc::now().timestamp() - started_at).max(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RagModelStatus {
    /// Auto-indexing is paused; takes priority over the model's load state.
    Paused,
    /// The embedding model is in memory (or actively being loaded for an
    /// in-flight indexing pass).
    Ready,
    /// The daemon is healthy but the model has unloaded after sitting idle;
    /// it reloads transparently on the next query or indexing pass.
    Sleeping,
    /// The configured provider is one this binary cannot serve (e.g. a local
    /// model on a build without the `local-embeddings` feature). Takes
    /// priority over every other signal — a capability gap, unlike pause or
    /// idle-unload, never resolves itself on the next query.
    Unavailable(&'static str),
    /// A local model's files are being fetched from HuggingFace Hub.
    Downloading { started_at: i64 },
    /// A local model's files are on disk; the ONNX session is being built.
    Preparing { started_at: i64 },
    /// The last download/prepare attempt for the configured local model
    /// failed; see the carried reason. Needs an explicit retry.
    DownloadFailed(String),
}

/// Maps the raw signals the CLI/TUI already read — the auto-index pause
/// flag, the persisted "is the model in memory" flag, and the count of
/// actively-processing queue items — into the truthful three-valued status
/// a user should see.
///
/// `processing_items > 0` always implies `Ready` even if `model_loaded` is
/// still false: the worker marks a queue item "processing" right before it
/// asks for the embedding client (see `IngestionManager::drain_queue`), so
/// there's a brief window where the model is being loaded but the persisted
/// flag hasn't landed yet. That window must never read as "sleeping".
pub fn compute_rag_model_status(
    paused: bool,
    model_loaded: bool,
    processing_items: i64,
) -> RagModelStatus {
    if paused {
        RagModelStatus::Paused
    } else if model_loaded || processing_items > 0 {
        RagModelStatus::Ready
    } else {
        RagModelStatus::Sleeping
    }
}

/// The truthful status a user should see, folding in whether this binary can
/// even serve `model` and whether it's still being acquired — not just
/// whether the daemon happens to have it loaded. Every surface that shows
/// RAG status (`canopy rag report`, the TUI sidebar/panel) must go through
/// this, not `compute_rag_model_status` directly, or a capability gap or an
/// in-progress download silently reads as "sleeping".
///
/// Priority: capability gap, then acquisition state, then pause/ready/sleep
/// — a download in progress is a more fundamental "can't do anything yet"
/// than auto-indexing being paused, so it wins over `paused` too.
pub fn compute_rag_status(
    model: &str,
    paused: bool,
    model_loaded: bool,
    processing_items: i64,
    acquisition: Option<AcquisitionState>,
) -> RagModelStatus {
    use crate::rag::embedding_client::{provider_available, provider_for_model, EmbeddingProvider};

    if provider_for_model(model) == Some(EmbeddingProvider::Local)
        && !provider_available(EmbeddingProvider::Local)
    {
        return RagModelStatus::Unavailable(
            crate::rag::embedding_client::LOCAL_EMBEDDINGS_UNAVAILABLE_REASON,
        );
    }

    match acquisition {
        Some(AcquisitionState::Downloading { started_at }) => {
            return RagModelStatus::Downloading { started_at }
        }
        Some(AcquisitionState::Preparing { started_at }) => {
            return RagModelStatus::Preparing { started_at }
        }
        Some(AcquisitionState::Failed { reason }) => return RagModelStatus::DownloadFailed(reason),
        None => {}
    }

    compute_rag_model_status(paused, model_loaded, processing_items)
}

/// Read the persisted "is the embedding model currently cached in the
/// daemon's memory" flag. A passive lookup — it can never itself trigger a
/// model load.
pub fn is_model_loaded(db: &Database) -> bool {
    db.get_state(RAG_MODEL_LOADED_KEY).ok().flatten().as_deref() == Some("1")
}

/// Unix timestamp of when the currently-cached model was loaded, if any.
pub fn model_loaded_since(db: &Database) -> Option<i64> {
    db.get_state(RAG_MODEL_SINCE_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_wins_regardless_of_model_state() {
        assert_eq!(
            compute_rag_model_status(true, true, 5),
            RagModelStatus::Paused
        );
        assert_eq!(
            compute_rag_model_status(true, false, 0),
            RagModelStatus::Paused
        );
    }

    #[test]
    fn ready_when_model_loaded() {
        assert_eq!(
            compute_rag_model_status(false, true, 0),
            RagModelStatus::Ready
        );
    }

    #[test]
    fn sleeping_when_idle_and_unloaded() {
        assert_eq!(
            compute_rag_model_status(false, false, 0),
            RagModelStatus::Sleeping
        );
    }

    #[test]
    fn cloud_model_status_ignores_capability_and_defers_to_daemon_state() {
        assert_eq!(
            compute_rag_status("text-embedding-3-small", false, true, 0, None),
            RagModelStatus::Ready
        );
        assert_eq!(
            compute_rag_status("text-embedding-3-small", false, false, 0, None),
            RagModelStatus::Sleeping
        );
    }

    #[test]
    #[cfg(not(feature = "local-embeddings"))]
    fn local_model_status_is_unavailable_without_the_feature_regardless_of_daemon_state() {
        // Even a "ready" daemon state (paused=false, loaded=true) must not
        // hide a capability gap — that's the exact silent-green defect.
        assert!(matches!(
            compute_rag_status("baai/bge-small-en-v1.5", false, true, 3, None),
            RagModelStatus::Unavailable(_)
        ));
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn local_model_status_defers_to_daemon_state_with_the_feature() {
        assert_eq!(
            compute_rag_status("baai/bge-small-en-v1.5", false, true, 0, None),
            RagModelStatus::Ready
        );
    }

    #[test]
    fn acquisition_state_wins_over_ready_daemon_state() {
        assert_eq!(
            compute_rag_status(
                "text-embedding-3-small",
                false,
                true,
                3,
                Some(AcquisitionState::Downloading { started_at: 100 }),
            ),
            RagModelStatus::Downloading { started_at: 100 }
        );
        assert_eq!(
            compute_rag_status(
                "text-embedding-3-small",
                false,
                true,
                3,
                Some(AcquisitionState::Preparing { started_at: 100 }),
            ),
            RagModelStatus::Preparing { started_at: 100 }
        );
        assert_eq!(
            compute_rag_status(
                "text-embedding-3-small",
                false,
                true,
                3,
                Some(AcquisitionState::Failed {
                    reason: "network error".to_string()
                }),
            ),
            RagModelStatus::DownloadFailed("network error".to_string())
        );
    }

    #[test]
    fn acquisition_state_wins_over_paused() {
        // A download in progress is more fundamental than auto-indexing
        // being paused — the model genuinely isn't usable yet either way.
        assert_eq!(
            compute_rag_status(
                "text-embedding-3-small",
                true,
                false,
                0,
                Some(AcquisitionState::Downloading { started_at: 0 }),
            ),
            RagModelStatus::Downloading { started_at: 0 }
        );
    }

    #[test]
    fn read_acquisition_state_absent_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        assert_eq!(read_acquisition_state(&db, "some-model"), None);
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn mark_downloading_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        mark_downloading(&db, "my-model");

        match read_acquisition_state(&db, "my-model") {
            Some(AcquisitionState::Downloading { started_at }) => {
                assert!(started_at > 0);
            }
            other => panic!("expected Downloading, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn mark_preparing_preserves_started_at_from_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        mark_downloading(&db, "my-model");
        let Some(AcquisitionState::Downloading {
            started_at: original,
        }) = read_acquisition_state(&db, "my-model")
        else {
            panic!("expected Downloading after mark_downloading");
        };

        mark_preparing(&db, "my-model");
        match read_acquisition_state(&db, "my-model") {
            Some(AcquisitionState::Preparing { started_at }) => {
                assert_eq!(started_at, original);
            }
            other => panic!("expected Preparing, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn mark_preparing_without_prior_downloading_sets_started_at() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        mark_preparing(&db, "my-model");

        match read_acquisition_state(&db, "my-model") {
            Some(AcquisitionState::Preparing { started_at }) => assert!(started_at > 0),
            other => panic!("expected Preparing, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn mark_failed_then_read_round_trips_reason() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        mark_failed(&db, "my-model", "connection reset");

        match read_acquisition_state(&db, "my-model") {
            Some(AcquisitionState::Failed { reason }) => assert_eq!(reason, "connection reset"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn clear_acquisition_makes_state_absent_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        mark_failed(&db, "my-model", "boom");
        assert!(read_acquisition_state(&db, "my-model").is_some());

        clear_acquisition(&db, "my-model");
        assert_eq!(read_acquisition_state(&db, "my-model"), None);
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn acquisition_states_are_tracked_independently_per_model() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        mark_downloading(&db, "model-a");
        mark_failed(&db, "model-b", "oops");

        assert!(matches!(
            read_acquisition_state(&db, "model-a"),
            Some(AcquisitionState::Downloading { .. })
        ));
        assert!(matches!(
            read_acquisition_state(&db, "model-b"),
            Some(AcquisitionState::Failed { .. })
        ));
    }

    #[test]
    fn acquisition_message_names_the_model_and_state() {
        let downloading = acquisition_message(
            "my-model",
            &AcquisitionState::Downloading {
                started_at: chrono::Utc::now().timestamp(),
            },
        );
        assert!(downloading.contains("my-model"));
        assert!(downloading.contains("downloading"));

        let preparing = acquisition_message(
            "my-model",
            &AcquisitionState::Preparing {
                started_at: chrono::Utc::now().timestamp(),
            },
        );
        assert!(preparing.contains("prepared"));

        let failed = acquisition_message(
            "my-model",
            &AcquisitionState::Failed {
                reason: "disk full".to_string(),
            },
        );
        assert!(failed.contains("disk full"));
        assert!(failed.contains("canopy rag model retry"));
    }

    #[test]
    fn active_processing_forces_ready_even_if_flag_not_yet_persisted() {
        assert_eq!(
            compute_rag_model_status(false, false, 3),
            RagModelStatus::Ready
        );
    }

    #[test]
    fn is_model_loaded_reads_persisted_flag() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        assert!(!is_model_loaded(&db));

        db.set_state(RAG_MODEL_LOADED_KEY, "1").unwrap();
        assert!(is_model_loaded(&db));

        db.set_state(RAG_MODEL_LOADED_KEY, "0").unwrap();
        assert!(!is_model_loaded(&db));
    }

    #[test]
    fn model_loaded_since_parses_persisted_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        assert_eq!(model_loaded_since(&db), None);

        db.set_state(RAG_MODEL_SINCE_KEY, "1752300000").unwrap();
        assert_eq!(model_loaded_since(&db), Some(1752300000));
    }
}
