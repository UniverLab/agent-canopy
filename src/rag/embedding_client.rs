#![allow(dead_code)]

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::canopy_config::CanopyConfig;

const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_GEMINI_DIMENSIONS: usize = 3072;

pub trait EmbeddingClient: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

pub fn client_from_config(config: &CanopyConfig) -> Result<Box<dyn EmbeddingClient>> {
    let model = config.embeddings_model.trim();
    if model.is_empty() {
        bail!("Embeddings model is not configured");
    }

    match provider_for_model(model) {
        Some(EmbeddingProvider::OpenAi) => Ok(Box::new(OpenAIEmbeddingClient::from_env(model)?)),
        Some(EmbeddingProvider::Gemini) => Ok(Box::new(GeminiEmbeddingClient::from_env(model)?)),
        #[cfg(feature = "local-embeddings")]
        Some(EmbeddingProvider::Local) => {
            let cache_dir = dirs::home_dir()
                .ok_or_else(|| anyhow!("No home directory found"))?
                .join(".canopy")
                .join("models");
            Ok(Box::new(LocalEmbeddingClient::new(model, &cache_dir)?))
        }
        #[cfg(not(feature = "local-embeddings"))]
        Some(EmbeddingProvider::Local) => {
            bail!("Local embeddings unavailable: {LOCAL_EMBEDDINGS_UNAVAILABLE_REASON}")
        }
        None => bail!("Unsupported embeddings model: {model}"),
    }
}

/// Reason shown wherever a build cannot serve local embedding models —
/// doctor, setup, RAG status, and this module's own error path all share
/// this string so the explanation is identical everywhere it surfaces.
pub const LOCAL_EMBEDDINGS_UNAVAILABLE_REASON: &str =
    "this canopy binary was built without the 'local-embeddings' feature (no ONNX Runtime support)";

/// Whether this binary can actually serve the given provider, as opposed to
/// whether a model string merely names it. Doctor/setup/RAG-status must all
/// route through this single check instead of sprinkling their own
/// `cfg!(feature = "local-embeddings")` — that duplication is exactly how the
/// capability/configuration gap re-opens.
pub fn provider_available(provider: EmbeddingProvider) -> bool {
    match provider {
        EmbeddingProvider::Local => cfg!(feature = "local-embeddings"),
        EmbeddingProvider::OpenAi | EmbeddingProvider::Gemini => true,
    }
}

pub fn model_dimensions(model: &str) -> Result<usize> {
    if let Some(dimensions) = parse_dimension_suffix(model) {
        return Ok(dimensions);
    }

    match model.trim().to_ascii_lowercase().as_str() {
        "text-embedding-3-small" => Ok(1536),
        "text-embedding-3-large" => Ok(3072),
        "text-embedding-ada-002" => Ok(1536),
        "gemini-embedding-001" | "gemini-embedding-2" => Ok(DEFAULT_GEMINI_DIMENSIONS),
        // Local models via fastembed
        "baai/bge-small-en-v1.5" => Ok(384),
        "baai/bge-base-en-v1.5" => Ok(768),
        "baai/bge-large-en-v1.5" => Ok(1024),
        "intfloat/multilingual-e5-small" => Ok(384),
        "intfloat/multilingual-e5-base" => Ok(768),
        "intfloat/multilingual-e5-large" => Ok(1024),
        _ => bail!("Unsupported embeddings model: {model}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProvider {
    OpenAi,
    Gemini,
    Local,
}

pub fn provider_for_model(model: &str) -> Option<EmbeddingProvider> {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }

    if normalized.contains("gemini") || normalized == "embedding-001" {
        Some(EmbeddingProvider::Gemini)
    } else if normalized.starts_with("text-embedding") || normalized.contains("openai") {
        Some(EmbeddingProvider::OpenAi)
    } else if LOCAL_MODEL_IDS.contains(&normalized.as_str()) {
        Some(EmbeddingProvider::Local)
    } else {
        None
    }
}

/// Canonical IDs of supported local (fastembed/ONNX) embedding models.
pub const LOCAL_MODEL_IDS: &[&str] = &[
    "baai/bge-small-en-v1.5",
    "baai/bge-base-en-v1.5",
    "baai/bge-large-en-v1.5",
    "intfloat/multilingual-e5-small",
    "intfloat/multilingual-e5-base",
    "intfloat/multilingual-e5-large",
];

pub struct OpenAIEmbeddingClient {
    client: reqwest::blocking::Client,
    api_key: String,
    model: String,
    base_url: String,
    dimensions: usize,
}

impl OpenAIEmbeddingClient {
    const API_KEY_ENV: &'static str = "OPENAI_API_KEY";

    pub fn from_env(model: &str) -> Result<Self> {
        let api_key = std::env::var(Self::API_KEY_ENV)
            .with_context(|| format!("{} is not set", Self::API_KEY_ENV))?;
        Self::new(model, &api_key)
    }

    pub fn new(model: &str, api_key: &str) -> Result<Self> {
        Self::with_base_url(model, api_key, OPENAI_BASE_URL)
    }

    fn with_base_url(model: &str, api_key: &str, base_url: &str) -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .user_agent("canopy-rag")
                .build()
                .context("Failed to build OpenAI embeddings client")?,
            api_key: api_key.to_owned(),
            model: model.to_owned(),
            base_url: base_url.to_owned(),
            dimensions: model_dimensions(model)?,
        })
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
}

impl EmbeddingClient for OpenAIEmbeddingClient {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let response = self
            .client
            .post(format!(
                "{}/embeddings",
                self.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .json(&OpenAIEmbeddingRequest {
                model: &self.model,
                input: text,
            })
            .send()
            .context("Failed to call OpenAI embeddings API")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("OpenAI embeddings API returned {status}: {body}");
        }

        let payload: OpenAIEmbeddingResponse = response
            .json()
            .context("Failed to parse OpenAI embeddings response")?;
        let embedding = payload
            .data
            .into_iter()
            .next()
            .map(|item| item.embedding)
            .filter(|embedding| !embedding.is_empty())
            .ok_or_else(|| anyhow!("OpenAI embeddings response did not contain an embedding"))?;

        validate_embedding_dimensions(&self.model, self.dimensions, &embedding)?;
        Ok(embedding)
    }
}

pub struct GeminiEmbeddingClient {
    client: reqwest::blocking::Client,
    api_key: String,
    model: String,
    base_url: String,
    dimensions: usize,
}

impl GeminiEmbeddingClient {
    const API_KEY_ENV: &'static str = "GEMINI_API_KEY";

    pub fn from_env(model: &str) -> Result<Self> {
        let api_key = std::env::var(Self::API_KEY_ENV)
            .with_context(|| format!("{} is not set", Self::API_KEY_ENV))?;
        Self::new(model, &api_key)
    }

    pub fn new(model: &str, api_key: &str) -> Result<Self> {
        Self::with_base_url(model, api_key, GEMINI_BASE_URL)
    }

    fn with_base_url(model: &str, api_key: &str, base_url: &str) -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .user_agent("canopy-rag")
                .build()
                .context("Failed to build Gemini embeddings client")?,
            api_key: api_key.to_owned(),
            model: model.to_owned(),
            base_url: base_url.to_owned(),
            dimensions: model_dimensions(model)?,
        })
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn endpoint_model(&self) -> String {
        if self.model.starts_with("models/") {
            self.model.clone()
        } else {
            format!("models/{}", self.model)
        }
    }
}

impl EmbeddingClient for GeminiEmbeddingClient {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let response = self
            .client
            .post(format!(
                "{}/v1beta/{}:embedContent?key={}",
                self.base_url.trim_end_matches('/'),
                self.endpoint_model(),
                self.api_key
            ))
            .json(&GeminiEmbeddingRequest {
                content: GeminiContent {
                    parts: vec![GeminiPart { text }],
                },
                output_dimensionality: self.dimensions,
            })
            .send()
            .context("Failed to call Gemini embeddings API")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("Gemini embeddings API returned {status}: {body}");
        }

        let payload: GeminiEmbeddingResponse = response
            .json()
            .context("Failed to parse Gemini embeddings response")?;
        let embedding = payload
            .embedding
            .map(|embedding| embedding.values)
            .filter(|embedding| !embedding.is_empty())
            .ok_or_else(|| anyhow!("Gemini embeddings response did not contain an embedding"))?;

        validate_embedding_dimensions(&self.model, self.dimensions, &embedding)?;
        Ok(embedding)
    }
}

/// Embedding client backed by a local ONNX model via fastembed.
///
/// The model file is downloaded from HuggingFace on first use and cached in
/// `~/.canopy/models/`. No API key is required.
#[cfg(feature = "local-embeddings")]
pub struct LocalEmbeddingClient {
    // fastembed::TextEmbedding is Send but not Sync; wrapping in Mutex makes
    // the struct Sync so it satisfies the EmbeddingClient bound.
    model: std::sync::Mutex<fastembed::TextEmbedding>,
    dimensions: usize,
}

#[cfg(feature = "local-embeddings")]
impl LocalEmbeddingClient {
    pub fn new(model_id: &str, cache_dir: &std::path::Path) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            // `super` rather than `crate::rag` so this also resolves when
            // `examples/rag_search.rs` pulls this file in as a flat module.
            let ort_path = super::ort_runtime::ensure_ort_runtime()
                .context("ONNX Runtime not available for local embeddings")?;
            std::env::set_var("ORT_DYLIB_PATH", &ort_path);
        }

        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("Cannot create model cache dir: {}", cache_dir.display()))?;

        let fastembed_model = model_id_to_fastembed(model_id)?;
        let dimensions = model_dimensions(model_id)?;

        tracing::info!(
            "Loading local embedding model '{model_id}' (cache: {})",
            cache_dir.display()
        );
        let text_embedding = fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(fastembed_model)
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(false),
        )
        .with_context(|| format!("Failed to load local embedding model '{model_id}'"))?;

        Ok(Self {
            model: std::sync::Mutex::new(text_embedding),
            dimensions,
        })
    }
}

#[cfg(feature = "local-embeddings")]
impl EmbeddingClient for LocalEmbeddingClient {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut model = self
            .model
            .lock()
            .map_err(|_| anyhow!("LocalEmbeddingClient mutex poisoned"))?;
        let mut embeddings = model
            .embed(vec![text], None)
            .context("Local embedding inference failed")?;
        let embedding = embeddings
            .pop()
            .filter(|e| !e.is_empty())
            .ok_or_else(|| anyhow!("Local embedding model returned no output"))?;
        validate_embedding_dimensions(
            model_id_for_dimensions(self.dimensions),
            self.dimensions,
            &embedding,
        )?;
        Ok(embedding)
    }
}

/// Whether `model_id` is already fully cached on disk. A fast, disk-only
/// check — no network access — so it's safe to call from an interactive
/// process like the setup wizard without risking a blocking download; the
/// wizard uses this to decide whether to say "ready now" or "downloading in
/// the background" without ever downloading anything itself (the daemon's
/// `IngestionManager` owns that, via
/// `rag::model_acquisition::acquire_local_model`).
#[cfg(feature = "local-embeddings")]
pub fn is_local_model_cached(model_id: &str, cache_dir: &std::path::Path) -> Result<bool> {
    let fastembed_model = model_id_to_fastembed(model_id)?;
    local_model_is_cached(&fastembed_model, cache_dir)
}

/// Cheap, on-disk check for whether every file `TextEmbedding::try_new`
/// would need for `model` is already present in `cache_dir` — without
/// constructing the model, which loads the ONNX graph and initialises the
/// runtime (observed ~5.7s for multilingual-e5-base) even when nothing
/// actually needs downloading.
///
/// This mirrors `fastembed::text_embedding::TextEmbedding::try_new` exactly:
/// the model's own file(s) (`ModelInfo::model_file` +
/// `ModelInfo::additional_files`) plus the four tokenizer files
/// `load_tokenizer_hf_hub` reads. `hf_hub::Cache::get` is the same lookup
/// fastembed's own `pull_from_hf` uses internally to resolve a cached path,
/// so this check is exact, not a heuristic — there is no "can't tell
/// cheaply" fallback here because none is needed. It also honors `HF_HOME`
/// the same way `pull_from_hf` does, so the check stays accurate when that
/// env var overrides the cache directory.
#[cfg(feature = "local-embeddings")]
pub(crate) fn local_model_is_cached(
    model: &fastembed::EmbeddingModel,
    cache_dir: &std::path::Path,
) -> Result<bool> {
    let info = fastembed::TextEmbedding::get_model_info(model)?;

    let effective_cache_dir = std::env::var("HF_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| cache_dir.to_path_buf());
    let repo = hf_hub::Cache::new(effective_cache_dir).model(info.model_code.clone());

    let mut required_files: Vec<&str> = vec![info.model_file.as_str()];
    required_files.extend(info.additional_files.iter().map(String::as_str));
    required_files.extend([
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ]);

    Ok(required_files.iter().all(|file| repo.get(file).is_some()))
}

#[cfg(feature = "local-embeddings")]
pub(crate) fn model_id_to_fastembed(model_id: &str) -> Result<fastembed::EmbeddingModel> {
    match model_id.trim().to_ascii_lowercase().as_str() {
        "baai/bge-small-en-v1.5" => Ok(fastembed::EmbeddingModel::BGESmallENV15),
        "baai/bge-base-en-v1.5" => Ok(fastembed::EmbeddingModel::BGEBaseENV15),
        "baai/bge-large-en-v1.5" => Ok(fastembed::EmbeddingModel::BGELargeENV15),
        "intfloat/multilingual-e5-small" => Ok(fastembed::EmbeddingModel::MultilingualE5Small),
        "intfloat/multilingual-e5-base" => Ok(fastembed::EmbeddingModel::MultilingualE5Base),
        "intfloat/multilingual-e5-large" => Ok(fastembed::EmbeddingModel::MultilingualE5Large),
        _ => bail!("No fastembed mapping for local model '{model_id}'"),
    }
}

fn model_id_for_dimensions(dimensions: usize) -> &'static str {
    match dimensions {
        384 => "local-384d",
        768 => "local-768d",
        1024 => "local-1024d",
        _ => "local",
    }
}

#[derive(Debug, Clone)]
pub struct MockEmbeddingClient {
    dimensions: usize,
}

impl MockEmbeddingClient {
    pub fn new(dimensions: usize) -> Self {
        Self { dimensions }
    }
}

impl EmbeddingClient for MockEmbeddingClient {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if self.dimensions == 0 {
            bail!("Mock embedding dimensions must be greater than zero");
        }

        let mut embedding = vec![0.0; self.dimensions];
        for (index, token) in text
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|token| !token.is_empty())
            .enumerate()
        {
            let bucket = index % self.dimensions;
            let weight = token.bytes().map(f32::from).sum::<f32>().max(1.0);
            embedding[bucket] += weight;
        }

        if embedding.iter().all(|value| *value == 0.0) {
            embedding[0] = 1.0;
        }

        Ok(normalize_embedding(embedding))
    }
}

fn parse_dimension_suffix(model: &str) -> Option<usize> {
    let suffix = model.trim().rsplit_once('-')?.1;
    let dimensions = suffix.strip_suffix('d')?;
    dimensions.parse().ok().filter(|value| *value > 0)
}

fn validate_embedding_dimensions(
    model: &str,
    expected_dimensions: usize,
    embedding: &[f32],
) -> Result<()> {
    if embedding.len() == expected_dimensions {
        return Ok(());
    }

    bail!(
        "Embedding dimensions mismatch for {model}: expected {expected_dimensions}, got {}",
        embedding.len()
    )
}

fn normalize_embedding(values: Vec<f32>) -> Vec<f32> {
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm == 0.0 {
        values
    } else {
        values.into_iter().map(|value| value / norm).collect()
    }
}

#[derive(Serialize)]
struct OpenAIEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Deserialize)]
struct OpenAIEmbeddingResponse {
    data: Vec<OpenAIEmbeddingData>,
}

#[derive(Deserialize)]
struct OpenAIEmbeddingData {
    embedding: Vec<f32>,
}

#[derive(Serialize)]
struct GeminiEmbeddingRequest<'a> {
    content: GeminiContent<'a>,
    #[serde(rename = "outputDimensionality")]
    output_dimensionality: usize,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct GeminiEmbeddingResponse {
    embedding: Option<GeminiEmbedding>,
}

#[derive(Deserialize)]
struct GeminiEmbedding {
    values: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    #[test]
    fn model_dimensions_parses_known_models_and_suffixes() {
        assert_eq!(model_dimensions("text-embedding-3-small").unwrap(), 1536);
        assert_eq!(model_dimensions("gemini-embedding-001").unwrap(), 3072);
        assert_eq!(model_dimensions("custom-768d").unwrap(), 768);
    }

    #[test]
    fn provider_for_model_detects_supported_models() {
        assert_eq!(
            provider_for_model("text-embedding-3-small"),
            Some(EmbeddingProvider::OpenAi)
        );
        assert_eq!(
            provider_for_model("gemini-embedding-001"),
            Some(EmbeddingProvider::Gemini)
        );
        assert_eq!(provider_for_model("custom-embedding"), None);
    }

    #[test]
    fn client_from_config_selects_supported_clients() {
        let openai_key = EnvGuard::set("OPENAI_API_KEY", "test-openai-key");
        let gemini_key = EnvGuard::set("GEMINI_API_KEY", "test-gemini-key");

        let mut config = CanopyConfig {
            embeddings_model: "text-embedding-3-small".to_string(),
            ..CanopyConfig::default()
        };
        assert!(client_from_config(&config).is_ok());

        config.embeddings_model = "gemini-embedding-001".to_string();
        assert!(client_from_config(&config).is_ok());

        drop(gemini_key);
        drop(openai_key);
    }

    #[test]
    fn client_from_config_rejects_missing_model() {
        let error = client_from_config(&CanopyConfig::default())
            .err()
            .expect("missing model should fail");
        assert!(error.to_string().contains("not configured"));
    }

    #[test]
    fn cloud_providers_are_always_available() {
        assert!(provider_available(EmbeddingProvider::OpenAi));
        assert!(provider_available(EmbeddingProvider::Gemini));
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn local_provider_available_when_feature_compiled_in() {
        assert!(provider_available(EmbeddingProvider::Local));
    }

    #[test]
    #[cfg(not(feature = "local-embeddings"))]
    fn local_provider_unavailable_without_feature() {
        assert!(!provider_available(EmbeddingProvider::Local));
    }

    #[test]
    #[cfg(not(feature = "local-embeddings"))]
    fn client_from_config_names_the_reason_for_local_without_feature() {
        let config = CanopyConfig {
            embeddings_model: LOCAL_MODEL_IDS[0].to_string(),
            ..Default::default()
        };
        let error = client_from_config(&config)
            .err()
            .expect("local model without the feature should fail");
        assert!(error
            .to_string()
            .contains(LOCAL_EMBEDDINGS_UNAVAILABLE_REASON));
    }

    #[test]
    fn mock_embedding_client_returns_normalized_embedding() {
        let client = MockEmbeddingClient::new(4);
        let embedding = client.embed("alpha beta alpha").unwrap();

        assert_eq!(embedding.len(), 4);
        let magnitude = embedding
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        assert!((magnitude - 1.0).abs() < 1e-5);
    }

    #[test]
    fn openai_embedding_client_embeds_text() {
        let response = r#"{"data":[{"embedding":[0.1,0.2,0.3,0.4]}]}"#;
        let (base_url, server) = serve_once(response);
        let client =
            OpenAIEmbeddingClient::with_base_url("custom-4d", "test-key", &base_url).unwrap();

        let embedding = client.embed("hello world").unwrap();
        let request = server.join().unwrap();

        assert_eq!(embedding, vec![0.1, 0.2, 0.3, 0.4]);
        assert!(request.contains("POST /embeddings HTTP/1.1"));
        assert!(request.contains("authorization: Bearer test-key"));
        assert!(request.contains("\"model\":\"custom-4d\""));
        assert!(request.contains("\"input\":\"hello world\""));
    }

    #[test]
    fn openai_embedding_client_rejects_dimension_mismatches() {
        let response = r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#;
        let (base_url, server) = serve_once(response);
        let client =
            OpenAIEmbeddingClient::with_base_url("custom-4d", "test-key", &base_url).unwrap();

        let error = client.embed("hello world").unwrap_err();
        let _ = server.join().unwrap();

        assert!(error.to_string().contains("expected 4, got 3"));
    }

    #[test]
    fn gemini_embedding_client_embeds_text() {
        let response = r#"{"embedding":{"values":[0.5,0.25,0.125,0.0625]}}"#;
        let (base_url, server) = serve_once(response);
        let client =
            GeminiEmbeddingClient::with_base_url("custom-4d", "gem-key", &base_url).unwrap();

        let embedding = client.embed("hello gemini").unwrap();
        let request = server.join().unwrap();

        assert_eq!(embedding, vec![0.5, 0.25, 0.125, 0.0625]);
        assert!(request.contains("POST /v1beta/models/custom-4d:embedContent?key=gem-key HTTP/1.1"));
        assert!(request.contains("\"outputDimensionality\":4"));
        assert!(request.contains("\"text\":\"hello gemini\""));
    }

    fn serve_once(response_body: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response_body = response_body.to_string();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let bytes_read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            request
        });

        (format!("http://{address}"), handle)
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_deref() {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    // ── parse_dimension_suffix ─────────────────────────────────────────

    #[test]
    fn parse_dimension_suffix_extracts_numeric_suffix() {
        assert_eq!(parse_dimension_suffix("model-768d"), Some(768));
        assert_eq!(parse_dimension_suffix("custom-1536d"), Some(1536));
        assert_eq!(parse_dimension_suffix("x-1d"), Some(1));
    }

    #[test]
    fn parse_dimension_suffix_ignores_non_numeric_suffix() {
        assert_eq!(parse_dimension_suffix("model-large"), None);
        assert_eq!(parse_dimension_suffix("model-base"), None);
        assert_eq!(parse_dimension_suffix("text-embedding-3-small"), None);
    }

    #[test]
    fn parse_dimension_suffix_rejects_zero_dimensions() {
        assert_eq!(parse_dimension_suffix("model-0d"), None);
    }

    #[test]
    fn parse_dimension_suffix_handles_whitespace() {
        assert_eq!(parse_dimension_suffix("  model-256d  "), Some(256));
    }

    #[test]
    fn parse_dimension_suffix_no_dash_means_none() {
        assert_eq!(parse_dimension_suffix("nodash"), None);
        assert_eq!(parse_dimension_suffix(""), None);
    }

    #[test]
    fn parse_dimension_suffix_suffix_without_d_is_none() {
        assert_eq!(parse_dimension_suffix("model-512"), None);
    }

    // ── model_id_for_dimensions ────────────────────────────────────────

    #[test]
    fn model_id_for_dimensions_returns_known_labels() {
        assert_eq!(model_id_for_dimensions(384), "local-384d");
        assert_eq!(model_id_for_dimensions(768), "local-768d");
        assert_eq!(model_id_for_dimensions(1024), "local-1024d");
    }

    #[test]
    fn model_id_for_dimensions_returns_fallback_for_unknown() {
        assert_eq!(model_id_for_dimensions(0), "local");
        assert_eq!(model_id_for_dimensions(999), "local");
        assert_eq!(model_id_for_dimensions(2048), "local");
    }

    // ── normalize_embedding ────────────────────────────────────────────

    #[test]
    fn normalize_embedding_produces_unit_vector() {
        let v = normalize_embedding(vec![3.0, 4.0]);
        let mag = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5);
        assert!((v[0] - 0.6).abs() < 1e-5);
        assert!((v[1] - 0.8).abs() < 1e-5);
    }

    #[test]
    fn normalize_embedding_preserves_zero_vector() {
        let v = normalize_embedding(vec![0.0, 0.0, 0.0]);
        assert_eq!(v, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn normalize_embedding_handles_single_element() {
        let v = normalize_embedding(vec![5.0]);
        assert!((v[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn normalize_embedding_handles_negative_values() {
        let v = normalize_embedding(vec![-3.0, 4.0]);
        let mag = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5);
        assert!(v[0] < 0.0);
        assert!(v[1] > 0.0);
    }

    // ── validate_embedding_dimensions ──────────────────────────────────

    #[test]
    fn validate_embedding_dimensions_ok_when_matching() {
        let result = validate_embedding_dimensions("model", 3, &[1.0, 2.0, 3.0]);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_embedding_dimensions_err_on_mismatch() {
        let result = validate_embedding_dimensions("model", 4, &[1.0, 2.0, 3.0]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("expected 4"));
        assert!(msg.contains("got 3"));
    }

    #[test]
    fn validate_embedding_dimensions_err_includes_model_name() {
        let result = validate_embedding_dimensions("my-model", 2, &[1.0]);
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("my-model"));
    }

    // ── model_dimensions edge cases ────────────────────────────────────

    #[test]
    fn model_dimensions_returns_error_for_unknown_model() {
        let result = model_dimensions("completely-unknown-model");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unsupported"));
    }

    #[test]
    fn model_dimensions_case_insensitive_for_known_models() {
        assert_eq!(model_dimensions("Text-Embedding-3-Small").unwrap(), 1536);
        assert_eq!(model_dimensions("TEXT-EMBEDDING-3-LARGE").unwrap(), 3072);
    }

    #[test]
    fn model_dimensions_all_local_models() {
        assert_eq!(model_dimensions("baai/bge-small-en-v1.5").unwrap(), 384);
        assert_eq!(model_dimensions("baai/bge-base-en-v1.5").unwrap(), 768);
        assert_eq!(model_dimensions("baai/bge-large-en-v1.5").unwrap(), 1024);
        assert_eq!(
            model_dimensions("intfloat/multilingual-e5-small").unwrap(),
            384
        );
        assert_eq!(
            model_dimensions("intfloat/multilingual-e5-base").unwrap(),
            768
        );
        assert_eq!(
            model_dimensions("intfloat/multilingual-e5-large").unwrap(),
            1024
        );
    }

    // ── provider_for_model edge cases ──────────────────────────────────

    #[test]
    fn provider_for_model_empty_string_returns_none() {
        assert_eq!(provider_for_model(""), None);
    }

    #[test]
    fn provider_for_model_whitespace_only_returns_none() {
        assert_eq!(provider_for_model("   "), None);
    }

    #[test]
    fn provider_for_model_gemini_embedding_001() {
        assert_eq!(
            provider_for_model("embedding-001"),
            Some(EmbeddingProvider::Gemini)
        );
    }

    #[test]
    fn provider_for_model_local_models_detected() {
        assert_eq!(
            provider_for_model("baai/bge-small-en-v1.5"),
            Some(EmbeddingProvider::Local)
        );
        assert_eq!(
            provider_for_model("intfloat/multilingual-e5-large"),
            Some(EmbeddingProvider::Local)
        );
    }

    #[test]
    fn provider_for_model_case_insensitive() {
        assert_eq!(
            provider_for_model("TEXT-EMBEDDING-3-SMALL"),
            Some(EmbeddingProvider::OpenAi)
        );
        assert_eq!(
            provider_for_model("Gemini-Embedding-001"),
            Some(EmbeddingProvider::Gemini)
        );
    }

    #[test]
    fn provider_for_model_with_whitespace() {
        assert_eq!(
            provider_for_model("  text-embedding-3-small  "),
            Some(EmbeddingProvider::OpenAi)
        );
    }

    #[test]
    fn provider_for_model_openai_keyword() {
        assert_eq!(
            provider_for_model("my-openai-model"),
            Some(EmbeddingProvider::OpenAi)
        );
    }

    // ── LOCAL_MODEL_IDS ────────────────────────────────────────────────

    #[test]
    fn local_model_ids_count() {
        assert_eq!(LOCAL_MODEL_IDS.len(), 6);
    }

    #[test]
    fn local_model_ids_are_unique() {
        let mut ids = LOCAL_MODEL_IDS.to_vec();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), LOCAL_MODEL_IDS.len());
    }

    /// The two halves of the local-model contract — our `LOCAL_MODEL_IDS` +
    /// `model_id_to_fastembed` mapping on one side, fastembed's own model
    /// catalog on the other — must agree. Without this, either list can
    /// drift silently: a fastembed upgrade that changes a model's dimensions
    /// (or drops a variant) would only surface as a runtime dimension
    /// mismatch deep in indexing, not here where it's cheap to catch.
    #[test]
    #[cfg(feature = "local-embeddings")]
    fn local_model_ids_agree_with_fastembed_catalog() {
        for id in LOCAL_MODEL_IDS {
            let fastembed_model = model_id_to_fastembed(id)
                .unwrap_or_else(|e| panic!("no fastembed mapping for '{id}': {e}"));
            let info =
                fastembed::TextEmbedding::get_model_info(&fastembed_model).unwrap_or_else(|e| {
                    panic!("fastembed has no info for the model mapped from '{id}': {e}")
                });
            let our_dims = model_dimensions(id).unwrap();
            assert_eq!(
                info.dim, our_dims,
                "dimension drift for '{id}': fastembed reports {}, we report {}",
                info.dim, our_dims
            );
        }
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn local_model_is_cached_false_when_cache_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !local_model_is_cached(&fastembed::EmbeddingModel::BGESmallENV15, dir.path()).unwrap()
        );
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn local_model_is_cached_false_when_only_partially_present() {
        let dir = tempfile::tempdir().unwrap();
        let model = fastembed::EmbeddingModel::BGESmallENV15;
        let info = fastembed::TextEmbedding::get_model_info(&model).unwrap();

        let repo = hf_hub::Cache::new(dir.path().to_path_buf()).model(info.model_code.clone());
        repo.create_ref("test-commit").unwrap();
        let snapshot_dir = repo.pointer_path("test-commit");

        // Only the model weight file, none of the tokenizer files.
        write_cached_file(&snapshot_dir, &info.model_file);

        assert!(!local_model_is_cached(&model, dir.path()).unwrap());
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn local_model_is_cached_true_once_every_required_file_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let model = fastembed::EmbeddingModel::BGESmallENV15;
        let info = fastembed::TextEmbedding::get_model_info(&model).unwrap();

        let repo = hf_hub::Cache::new(dir.path().to_path_buf()).model(info.model_code.clone());
        repo.create_ref("test-commit").unwrap();
        let snapshot_dir = repo.pointer_path("test-commit");

        write_cached_file(&snapshot_dir, &info.model_file);
        for extra in &info.additional_files {
            write_cached_file(&snapshot_dir, extra);
        }
        for tokenizer_file in [
            "tokenizer.json",
            "config.json",
            "special_tokens_map.json",
            "tokenizer_config.json",
        ] {
            write_cached_file(&snapshot_dir, tokenizer_file);
        }

        assert!(local_model_is_cached(&model, dir.path()).unwrap());
    }

    /// Writes a placeholder file at `snapshot_dir/relative_path`, creating
    /// any intermediate directories the path implies (some model files, e.g.
    /// "onnx/model.onnx", live in a subdirectory of the snapshot).
    #[cfg(feature = "local-embeddings")]
    fn write_cached_file(snapshot_dir: &std::path::Path, relative_path: &str) {
        let full_path = snapshot_dir.join(relative_path);
        std::fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        std::fs::write(full_path, b"fake").unwrap();
    }

    // ── MockEmbeddingClient ────────────────────────────────────────────

    #[test]
    fn mock_embedding_client_zero_dimensions_errors() {
        let client = MockEmbeddingClient::new(0);
        assert!(client.embed("hello").is_err());
    }

    #[test]
    fn mock_embedding_client_single_token() {
        let client = MockEmbeddingClient::new(3);
        let embedding = client.embed("hello").unwrap();
        assert_eq!(embedding.len(), 3);
        let mag = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5);
    }

    #[test]
    fn mock_embedding_client_empty_text() {
        let client = MockEmbeddingClient::new(3);
        let embedding = client.embed("").unwrap();
        assert_eq!(embedding.len(), 3);
        // Empty text: embedding[0] gets set to 1.0 then normalized
        assert!((embedding[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn mock_embedding_client_dimensions_match() {
        let client = MockEmbeddingClient::new(16);
        let embedding = client.embed("test").unwrap();
        assert_eq!(embedding.len(), 16);
    }

    // ── EmbeddingProvider equality ─────────────────────────────────────

    #[test]
    fn embedding_provider_eq_and_debug() {
        assert_eq!(EmbeddingProvider::OpenAi, EmbeddingProvider::OpenAi);
        assert_ne!(EmbeddingProvider::OpenAi, EmbeddingProvider::Gemini);
        assert_ne!(EmbeddingProvider::Gemini, EmbeddingProvider::Local);
        assert_ne!(EmbeddingProvider::OpenAi, EmbeddingProvider::Local);
        // Debug should not panic
        let _ = format!("{:?}", EmbeddingProvider::OpenAi);
    }

    #[test]
    fn embedding_provider_clone_and_copy() {
        let p = EmbeddingProvider::Gemini;
        let p2 = p;
        assert_eq!(p, p2);
    }

    // ── client_from_config edge cases ──────────────────────────────────

    #[test]
    fn client_from_config_whitespace_model_is_rejected() {
        let config = CanopyConfig {
            embeddings_model: "   ".to_string(),
            ..CanopyConfig::default()
        };
        let err = client_from_config(&config).err().unwrap();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn client_from_config_unsupported_model_is_rejected() {
        let config = CanopyConfig {
            embeddings_model: "unknown-model".to_string(),
            ..CanopyConfig::default()
        };
        let err = client_from_config(&config).err().unwrap();
        assert!(err.to_string().contains("Unsupported"));
    }
}
