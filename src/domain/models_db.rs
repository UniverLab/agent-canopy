//! Cached model catalog from <https://models.dev>.
//!
//! Provides a flat list of AI model entries with provider metadata,
//! cached locally for fast lookup.  The catalog can be filtered by
//! CLI name so the new-agent dialog only shows relevant models.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// How long the local cache stays valid before re-fetching.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const API_URL: &str = "https://models.dev/api.json";

// ── Public types ────────────────────────────────────────────────────

/// A single model entry with enough info for the picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Model identifier passed to the CLI (e.g. `claude-sonnet-4-6`).
    pub id: String,
    /// Human-readable name (e.g. `Claude Sonnet 4.6`).
    pub name: String,
    /// Provider slug (e.g. `anthropic`).
    pub provider: String,
    /// Release date from models.dev when available.
    pub release_date: Option<String>,
    /// Human-friendly size heuristic derived from the model identifier/name.
    pub size_hint: Option<String>,
}

/// Full catalog of models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub models: Vec<ModelEntry>,
    #[serde(with = "timestamp_serde")]
    pub fetched_at: SystemTime,
}

// ── CLI → provider mapping ──────────────────────────────────────────

/// Returns the models.dev provider slugs relevant for a given CLI.
pub fn providers_for_cli(cli: &str) -> &[&str] {
    match cli {
        "claude" => &["anthropic"],
        "codex" => &["openai"],
        "mistral" => &["mistral"],
        "copilot" => &[
            "openai",
            "anthropic",
            "google",
            "mistral",
            "xai",
            "deepseek",
        ],
        "gemini" => &["google"],
        "qwen" => &["alibaba"],
        "kiro" => &["anthropic", "amazon", "google"],
        // opencode supports any AI-SDK provider
        "opencode" => &[
            "anthropic",
            "openai",
            "google",
            "xai",
            "deepseek",
            "mistral",
            "amazon",
        ],
        _ => &[],
    }
}

// ── Cache path ──────────────────────────────────────────────────────

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".canopy/models_cache.json"))
}

// ── Public API ──────────────────────────────────────────────────────

/// Load the catalog from cache, fetching from the network if stale/missing.
///
/// Returns `None` only when both cache and network fail.
pub fn load_catalog() -> Option<ModelCatalog> {
    if let Some(cached) = load_from_cache() {
        if cached.fetched_at.elapsed().unwrap_or(CACHE_TTL) < CACHE_TTL {
            return Some(cached);
        }
    }
    // Cache stale or missing — try network
    fetch_and_cache().or_else(load_from_cache)
}

/// Load the catalog without ever blocking on the network.
///
/// Returns whatever cache exists (even stale) immediately; when the cache is
/// stale or missing, a background thread refreshes it for the next caller.
/// Use this from interactive paths (TUI dialogs) where a synchronous fetch
/// would freeze the UI for up to the request timeout.
pub fn load_catalog_nonblocking() -> Option<ModelCatalog> {
    let cached = load_from_cache();
    let fresh = cached
        .as_ref()
        .is_some_and(|c| c.fetched_at.elapsed().unwrap_or(CACHE_TTL) < CACHE_TTL);

    if !fresh {
        std::thread::spawn(|| {
            let _ = fetch_and_cache();
        });
    }

    cached
}

/// Models for `cli_name` matching `query` (case-insensitive substring),
/// in a single pass that only clones the matching entries.
pub fn suggestions_for(catalog: &ModelCatalog, cli_name: &str, query: &str) -> Vec<ModelEntry> {
    let providers = providers_for_cli(cli_name);
    let q = query.to_lowercase();
    catalog
        .models
        .iter()
        .filter(|m| providers.is_empty() || providers.contains(&m.provider.as_str()))
        .filter(|m| {
            q.is_empty() || m.id.to_lowercase().contains(&q) || m.name.to_lowercase().contains(&q)
        })
        .cloned()
        .collect()
}

// ── Internal: fetch ─────────────────────────────────────────────────

fn fetch_and_cache() -> Option<ModelCatalog> {
    let body: HashMap<String, ProviderRaw> = reqwest::blocking::Client::new()
        .get(API_URL)
        .timeout(Duration::from_secs(10))
        .send()
        .ok()?
        .json()
        .ok()?;

    let mut entries = Vec::new();
    for (provider_id, provider) in &body {
        for model in provider.models.values() {
            entries.push(ModelEntry {
                id: model.id.clone(),
                name: model.name.clone().unwrap_or_else(|| model.id.clone()),
                provider: provider_id.clone(),
                release_date: model.release_date.clone(),
                size_hint: infer_size_hint(&model.id, model.name.as_deref()),
            });
        }
    }
    entries.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.id.cmp(&b.id)));

    let catalog = ModelCatalog {
        models: entries,
        fetched_at: SystemTime::now(),
    };

    save_to_cache(&catalog);
    Some(catalog)
}

// ── Internal: cache I/O ─────────────────────────────────────────────

fn load_from_cache() -> Option<ModelCatalog> {
    let path = cache_path()?;
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_to_cache(catalog: &ModelCatalog) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(catalog) {
        let _ = std::fs::write(path, json);
    }
}

// ── Raw API types (only used for deserialization) ───────────────────

#[derive(Deserialize)]
struct ProviderRaw {
    #[serde(default)]
    models: HashMap<String, ModelRaw>,
}

#[derive(Deserialize)]
struct ModelRaw {
    id: String,
    name: Option<String>,
    release_date: Option<String>,
}

fn infer_size_hint(id: &str, name: Option<&str>) -> Option<String> {
    let combined = match name {
        Some(name) => format!("{id} {name}"),
        None => id.to_string(),
    };
    let lower = combined.to_lowercase();

    for token in lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '.') {
        if let Some(stripped) = token.strip_suffix('b') {
            if !stripped.is_empty() && stripped.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return Some(format!("{}B", stripped.to_uppercase()));
            }
        }
        if let Some(stripped) = token.strip_suffix('m') {
            if !stripped.is_empty() && stripped.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return Some(format!("{}M", stripped.to_uppercase()));
            }
        }
    }

    if lower.contains("small") {
        return Some("small".to_string());
    }
    if lower.contains("large") {
        return Some("large".to_string());
    }
    if lower.contains("base") {
        return Some("base".to_string());
    }

    None
}

// ── Timestamp serde helper ──────────────────────────────────────────

mod timestamp_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(time: &SystemTime, ser: S) -> Result<S::Ok, S::Error> {
        let secs = time
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        secs.serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<SystemTime, D::Error> {
        let secs = u64::deserialize(de)?;
        Ok(UNIX_EPOCH + Duration::from_secs(secs))
    }
}
