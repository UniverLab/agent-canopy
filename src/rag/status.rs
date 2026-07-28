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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RagModelStatus {
    /// Auto-indexing is paused; takes priority over the model's load state.
    Paused,
    /// The embedding model is in memory (or actively being loaded for an
    /// in-flight indexing pass).
    Ready,
    /// The daemon is healthy but the model has unloaded after sitting idle;
    /// it reloads transparently on the next query or indexing pass.
    Sleeping,
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
