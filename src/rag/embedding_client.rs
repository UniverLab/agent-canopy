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
        None => bail!("Unsupported embeddings model: {model}"),
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
        _ => bail!("Unsupported embeddings model: {model}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProvider {
    OpenAi,
    Gemini,
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
    } else {
        None
    }
}

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
}
