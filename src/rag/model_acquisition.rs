//! Background acquisition (download + first-load) of a local embedding
//! model, decoupled from the interactive setup wizard so choosing a model
//! never blocks on a multi-hundred-MB download.
//!
//! Kept in its own module — separate from `rag::embedding_client` — because
//! it's the one part of that file that needs `crate::db::Database` and
//! `crate::rag::status`; `examples/rag_search.rs` re-includes
//! `embedding_client.rs` standalone via `#[path]` and doesn't (and
//! shouldn't need to) pull in the rest of the crate to do it.

use anyhow::Result;

use crate::db::Database;
use crate::domain::canopy_config::CanopyConfig;
#[cfg(feature = "local-embeddings")]
use crate::rag::embedding_client::EmbeddingProvider;
use crate::rag::embedding_client::{self, EmbeddingClient};

/// Feature-independent entry point for `IngestionManager::get_embedding_client`
/// (the shared load point behind `rag_search` and indexing): routes a local
/// model through the download/prepare-tracked path when this build can serve
/// one, and falls back to `client_from_config`'s existing capability-gap
/// bail otherwise. Kept separate from `client_from_config` itself so the TUI
/// playground's throwaway-client path is unaffected by this change.
pub fn client_from_config_for_ingestion(
    config: &CanopyConfig,
    #[cfg_attr(not(feature = "local-embeddings"), allow(unused_variables))] db: &Database,
) -> Result<Box<dyn EmbeddingClient>> {
    #[cfg(feature = "local-embeddings")]
    {
        client_from_config_tracked(config, db)
    }
    #[cfg(not(feature = "local-embeddings"))]
    {
        embedding_client::client_from_config(config)
    }
}

/// If `model` is a local model this build can serve and isn't already
/// cached on disk, marks it "downloading" immediately — before
/// `IngestionManager` touches its client-cache lock at all. Without this, a
/// caller racing the one that will actually perform the (possibly
/// minutes-long) download could see no acquisition state yet, also try to
/// build the client, and block on that lock for the whole download instead
/// of bailing via `read_acquisition_state` — reintroducing the exact "silent
/// hang" this feature exists to remove. `mark_downloading` is idempotent, so
/// calling this and then having `acquire_local_model` mark it again is safe.
///
/// A no-op for a cloud model, an already-cached local model (the common
/// "daemon restarted, nothing to redownload" case), a build without
/// `local-embeddings`, or a model some other caller already claimed.
///
/// Note: the "is it already claimed" check and the claim itself are not one
/// atomic operation (two `daemon_state` round-trips, not a compare-and-swap)
/// — a caller arriving in that microsecond-scale gap could still race into
/// a duplicate attempt. Closing that fully would need a dedicated atomic
/// primitive in `Database`; given how narrow the window is (a couple of
/// SQLite round-trips, not a filesystem scan or network call), that's judged
/// not worth the added surface area here.
pub fn claim_local_download_if_needed(db: &Database, model: &str) {
    #[cfg(feature = "local-embeddings")]
    {
        if embedding_client::provider_for_model(model) != Some(EmbeddingProvider::Local) {
            return;
        }
        if crate::rag::status::read_acquisition_state(db, model).is_some() {
            return;
        }
        let Ok(fastembed_model) = embedding_client::model_id_to_fastembed(model) else {
            return;
        };
        let Some(cache_dir) = dirs::home_dir().map(|h| h.join(".canopy").join("models")) else {
            return;
        };
        if embedding_client::local_model_is_cached(&fastembed_model, &cache_dir).unwrap_or(false) {
            return;
        }
        crate::rag::status::mark_downloading(db, model);
    }
    #[cfg(not(feature = "local-embeddings"))]
    {
        let _ = (db, model);
    }
}

/// Like `client_from_config`, but for a local model additionally tracks
/// download/prepare state in `db` (see `rag::status`) so other processes —
/// the CLI, the TUI, a concurrent query racing the same load — see it's in
/// progress instead of either racing a duplicate attempt or reading a silent
/// gap as "sleeping". Used by `IngestionManager::get_embedding_client`, the
/// shared load point behind both `rag_search` and indexing;
/// `client_from_config` itself stays untouched (still an instant bail when a
/// local model isn't cached) for the TUI playground's throwaway-client path.
#[cfg(feature = "local-embeddings")]
pub fn client_from_config_tracked(
    config: &CanopyConfig,
    db: &Database,
) -> Result<Box<dyn EmbeddingClient>> {
    let model = config.embeddings_model.trim();
    if model.is_empty() {
        anyhow::bail!("Embeddings model is not configured");
    }

    match embedding_client::provider_for_model(model) {
        Some(EmbeddingProvider::OpenAi) => Ok(Box::new(
            embedding_client::OpenAIEmbeddingClient::from_env(model)?,
        )),
        Some(EmbeddingProvider::Gemini) => Ok(Box::new(
            embedding_client::GeminiEmbeddingClient::from_env(model)?,
        )),
        Some(EmbeddingProvider::Local) => Ok(Box::new(acquire_local_model(db, model)?)),
        None => anyhow::bail!("Unsupported embeddings model: {model}"),
    }
}

/// Ensures `model_id` is fully downloaded and loaded, tracking phase
/// transitions in `db` along the way. Cheap when already cached (skips
/// straight to loading, same as today); otherwise fetches the model's files
/// itself via `hf_hub` while marked "downloading", then builds the ONNX
/// session via fastembed — now instant, since everything it needs is
/// already on disk — while marked "preparing". Any failure is persisted as
/// "failed" with its reason rather than only returned, so it's visible to a
/// process other than the one that hit the error (the CLI, the TUI).
#[cfg(feature = "local-embeddings")]
pub(crate) fn acquire_local_model(
    db: &Database,
    model_id: &str,
) -> Result<embedding_client::LocalEmbeddingClient> {
    use anyhow::Context;

    let cache_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("No home directory found"))?
        .join(".canopy")
        .join("models");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("Cannot create model cache dir: {}", cache_dir.display()))?;

    let fastembed_model = embedding_client::model_id_to_fastembed(model_id)?;

    let acquire = || -> Result<embedding_client::LocalEmbeddingClient> {
        if !embedding_client::local_model_is_cached(&fastembed_model, &cache_dir)? {
            crate::rag::status::mark_downloading(db, model_id);
            download_required_files(&fastembed_model, &cache_dir)?;
        }
        crate::rag::status::mark_preparing(db, model_id);
        embedding_client::LocalEmbeddingClient::new(model_id, &cache_dir)
    };

    match acquire() {
        Ok(client) => {
            crate::rag::status::clear_acquisition(db, model_id);
            Ok(client)
        }
        Err(e) => {
            crate::rag::status::mark_failed(db, model_id, &format!("{e:#}"));
            Err(e)
        }
    }
}

/// Fetches every file `fastembed::TextEmbedding::try_new` would download for
/// `model`, using `hf_hub`'s blocking client directly — bypassing
/// fastembed's own loader (which bundles the download and the ONNX session
/// build into one opaque call) so the download phase is separately
/// observable from the "preparing" phase that follows. Mirrors
/// `fastembed::common::pull_from_hf`'s `HF_HOME`/`HF_ENDPOINT` handling, and
/// fetches exactly the file list `local_model_is_cached` already checks.
#[cfg(feature = "local-embeddings")]
fn download_required_files(
    model: &fastembed::EmbeddingModel,
    cache_dir: &std::path::Path,
) -> Result<()> {
    use anyhow::Context;

    let info = fastembed::TextEmbedding::get_model_info(model)?;

    let effective_cache_dir = std::env::var("HF_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| cache_dir.to_path_buf());
    let endpoint =
        std::env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".to_string());

    let api = hf_hub::api::sync::ApiBuilder::new()
        .with_cache_dir(effective_cache_dir)
        .with_endpoint(endpoint)
        // No terminal attached to show a progress bar to — this runs in the
        // daemon. `rag::status`'s downloading/preparing phases are the
        // progress signal instead.
        .with_progress(false)
        .build()
        .context("Failed to build HuggingFace Hub API client")?;
    let repo = api.model(info.model_code.clone());

    let mut files: Vec<&str> = vec![info.model_file.as_str()];
    files.extend(info.additional_files.iter().map(String::as_str));
    files.extend([
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ]);

    for file in files {
        repo.get(file).with_context(|| {
            format!(
                "Failed to download '{file}' for model '{}'",
                info.model_code
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `claim_local_download_if_needed` hardcodes `~/.canopy/models` as the
    /// cache dir (matching every other local-model call site), so these
    /// tests only cover the branches that return before ever touching it —
    /// the cache-hit/cache-miss branches aren't safely unit-testable without
    /// depending on this machine's real cache state.
    #[test]
    fn claim_local_download_is_a_no_op_for_a_cloud_model() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        claim_local_download_if_needed(&db, "text-embedding-3-small");

        assert_eq!(
            crate::rag::status::read_acquisition_state(&db, "text-embedding-3-small"),
            None
        );
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn claim_local_download_is_a_no_op_when_already_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        crate::rag::status::mark_failed(&db, "baai/bge-small-en-v1.5", "boom");

        claim_local_download_if_needed(&db, "baai/bge-small-en-v1.5");

        // Still "failed", not overwritten into "downloading" — a caller
        // must retry explicitly, not have this silently reattempt it.
        assert!(matches!(
            crate::rag::status::read_acquisition_state(&db, "baai/bge-small-en-v1.5"),
            Some(crate::rag::status::AcquisitionState::Failed { .. })
        ));
    }

    #[test]
    #[cfg(not(feature = "local-embeddings"))]
    fn claim_local_download_is_a_no_op_without_the_feature() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        claim_local_download_if_needed(&db, "baai/bge-small-en-v1.5");

        assert_eq!(
            crate::rag::status::read_acquisition_state(&db, "baai/bge-small-en-v1.5"),
            None
        );
    }
}
