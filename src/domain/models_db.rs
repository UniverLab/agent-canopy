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

/// Where a served catalog came from, so callers can flag staleness to users
/// instead of silently presenting possibly-outdated data as current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogSource {
    /// Freshly fetched from models.dev on this call.
    Live,
    /// Served from a still-fresh local cache — no network was touched.
    Cache,
    /// Served from a local cache that is past its TTL (or a forced refresh
    /// that failed to reach models.dev): usable, but possibly out of date.
    Stale,
}

impl CatalogSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CatalogSource::Live => "live",
            CatalogSource::Cache => "cache",
            CatalogSource::Stale => "stale",
        }
    }
}

/// A loaded catalog together with its provenance.
#[derive(Debug, Clone)]
pub struct CatalogLoad {
    pub catalog: ModelCatalog,
    pub source: CatalogSource,
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
        "kiro" => &["anthropic", "amazon-bedrock", "google"],
        // opencode is a universal gateway: besides the major foundation-model
        // providers it also exposes its own hosted catalog under the
        // `opencode`/`opencode-go` provider slugs (where its zen models, e.g.
        // `big-pickle`, live). Slugs verified against a live models.dev
        // snapshot — `amazon-bedrock`, not `amazon`, is the real slug.
        "opencode" => &[
            "anthropic",
            "openai",
            "google",
            "xai",
            "deepseek",
            "mistral",
            "amazon-bedrock",
            "opencode",
            "opencode-go",
        ],
        _ => &[],
    }
}

// ── Cache path ──────────────────────────────────────────────────────

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".canopy/models_cache.json"))
}

// ── Public API ──────────────────────────────────────────────────────

/// Load the catalog and report where it came from (`live`/`cache`/`stale`).
///
/// Policy:
/// - `force_refresh == false`: a fresh cache (within [`CACHE_TTL`]) is served
///   as [`CatalogSource::Cache`] with **no network call** — the hot path. A
///   stale or missing cache triggers a fetch ([`CatalogSource::Live`]); if the
///   fetch fails but a cache exists, it is served as [`CatalogSource::Stale`]
///   rather than failing hard.
/// - `force_refresh == true`: always fetch. On success the result is
///   [`CatalogSource::Live`] (and newly published models — e.g. opencode's
///   `big-pickle` — appear immediately); on failure any existing cache is
///   served as [`CatalogSource::Stale`].
///
/// Returns `None` only when there is neither a usable cache nor a reachable
/// models.dev.
pub fn load_catalog_with_source(force_refresh: bool) -> Option<CatalogLoad> {
    resolve_catalog(force_refresh, load_from_cache, fetch_and_cache)
}

/// TTL-and-provenance core of [`load_catalog_with_source`], with the cache read
/// and the network fetch injected so the staleness policy is testable without
/// touching disk or the network.
fn resolve_catalog(
    force_refresh: bool,
    load_cache: impl FnOnce() -> Option<ModelCatalog>,
    fetch: impl FnOnce() -> Option<ModelCatalog>,
) -> Option<CatalogLoad> {
    let cached = load_cache();

    if !force_refresh {
        if let Some(catalog) = cached.as_ref().filter(|c| is_fresh(c)).cloned() {
            return Some(CatalogLoad {
                catalog,
                source: CatalogSource::Cache,
            });
        }
    }

    // Either forced, or the cache is stale/missing: the network is the only
    // way to get current data. Never fail hard while a cache exists.
    match fetch() {
        Some(catalog) => Some(CatalogLoad {
            catalog,
            source: CatalogSource::Live,
        }),
        None => cached.map(|catalog| CatalogLoad {
            catalog,
            source: CatalogSource::Stale,
        }),
    }
}

/// Whether a cached catalog is still within its TTL. A timestamp in the future
/// (clock skew) makes `elapsed()` error; treat that as stale so a bad clock
/// forces a refresh rather than pinning a possibly-wrong cache forever.
fn is_fresh(catalog: &ModelCatalog) -> bool {
    catalog.fetched_at.elapsed().unwrap_or(CACHE_TTL) < CACHE_TTL
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

    let catalog = parse_catalog(body, SystemTime::now());
    save_to_cache(&catalog);
    Some(catalog)
}

/// Map the raw models.dev payload into a sorted [`ModelCatalog`]. Pure (no
/// network, no disk) so the provider→model mapping — including newly published
/// models appearing under a provider — is unit-testable with a mocked payload.
fn parse_catalog(body: HashMap<String, ProviderRaw>, fetched_at: SystemTime) -> ModelCatalog {
    let mut entries = Vec::new();
    for (provider_id, provider) in body {
        for model in provider.models.into_values() {
            let size_hint = infer_size_hint(&model.id, model.name.as_deref());
            let name = model.name.unwrap_or_else(|| model.id.clone());
            entries.push(ModelEntry {
                id: model.id,
                name,
                provider: provider_id.clone(),
                release_date: model.release_date,
                size_hint,
            });
        }
    }
    entries.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.id.cmp(&b.id)));

    ModelCatalog {
        models: entries,
        fetched_at,
    }
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

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    fn catalog(ids: &[(&str, &str)], age: Duration) -> ModelCatalog {
        ModelCatalog {
            models: ids
                .iter()
                .map(|(provider, id)| ModelEntry {
                    id: (*id).to_string(),
                    name: (*id).to_string(),
                    provider: (*provider).to_string(),
                    release_date: None,
                    size_hint: None,
                })
                .collect(),
            fetched_at: SystemTime::now() - age,
        }
    }

    fn ids(load: &CatalogLoad) -> Vec<String> {
        load.catalog.models.iter().map(|m| m.id.clone()).collect()
    }

    #[test]
    fn fresh_cache_is_served_as_cache_without_fetching() {
        let fetched = Cell::new(false);
        let load = resolve_catalog(
            false,
            || {
                Some(catalog(
                    &[("anthropic", "claude-x")],
                    Duration::from_secs(60),
                ))
            },
            || {
                fetched.set(true);
                None
            },
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Cache);
        assert!(!fetched.get(), "a fresh cache must not touch the network");
    }

    #[test]
    fn stale_cache_triggers_fetch_and_reports_live() {
        let load = resolve_catalog(
            false,
            || {
                Some(catalog(
                    &[("anthropic", "old")],
                    CACHE_TTL + Duration::from_secs(1),
                ))
            },
            || Some(catalog(&[("anthropic", "new")], Duration::ZERO)),
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Live);
        assert_eq!(ids(&load), vec!["new".to_string()]);
    }

    #[test]
    fn stale_cache_with_failed_fetch_is_served_stale_not_hard_failure() {
        let load = resolve_catalog(
            false,
            || {
                Some(catalog(
                    &[("anthropic", "old")],
                    CACHE_TTL + Duration::from_secs(1),
                ))
            },
            || None,
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Stale);
        assert_eq!(ids(&load), vec!["old".to_string()]);
    }

    #[test]
    fn no_cache_and_failed_fetch_returns_none() {
        let load = resolve_catalog(false, || None, || None);
        assert!(load.is_none());
    }

    #[test]
    fn force_refresh_fetches_even_when_cache_is_fresh() {
        let fetched = Cell::new(false);
        let load = resolve_catalog(
            true,
            || Some(catalog(&[("anthropic", "cached")], Duration::from_secs(1))),
            || {
                fetched.set(true);
                Some(catalog(&[("anthropic", "fresh")], Duration::ZERO))
            },
        )
        .unwrap();
        assert!(fetched.get(), "force_refresh must always fetch");
        assert_eq!(load.source, CatalogSource::Live);
        assert_eq!(ids(&load), vec!["fresh".to_string()]);
    }

    #[test]
    fn force_refresh_falls_back_to_stale_cache_when_fetch_fails() {
        let load = resolve_catalog(
            true,
            || Some(catalog(&[("anthropic", "cached")], Duration::from_secs(1))),
            || None,
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Stale);
        assert_eq!(ids(&load), vec!["cached".to_string()]);
    }

    #[test]
    fn newly_published_model_appears_only_after_a_refresh() {
        // A fresh cache that predates a newly-published model does NOT surface
        // it (served as-is, no network) — this is exactly why a force-refresh
        // path is needed.
        let old_cache = || {
            Some(catalog(
                &[("opencode", "north-mini-code-free")],
                Duration::from_secs(60),
            ))
        };
        let fetch_with_pickle = || {
            let body: HashMap<String, ProviderRaw> = serde_json::from_value(serde_json::json!({
                "opencode": {
                    "models": {
                        "big-pickle": { "id": "big-pickle", "name": "Big Pickle", "release_date": "2025-10-17" },
                        "north-mini-code-free": { "id": "north-mini-code-free" }
                    }
                }
            }))
            .unwrap();
            Some(parse_catalog(body, SystemTime::now()))
        };

        let cached = resolve_catalog(false, old_cache, fetch_with_pickle).unwrap();
        assert_eq!(cached.source, CatalogSource::Cache);
        assert!(
            !ids(&cached).contains(&"big-pickle".to_string()),
            "a fresh cache should not yet know about the new model"
        );

        // Forcing a refresh pulls it in from models.dev.
        let refreshed = resolve_catalog(true, old_cache, fetch_with_pickle).unwrap();
        assert_eq!(refreshed.source, CatalogSource::Live);
        let entry = refreshed
            .catalog
            .models
            .iter()
            .find(|m| m.id == "big-pickle")
            .expect("big-pickle must appear after a refresh");
        assert_eq!(entry.provider, "opencode");
        assert_eq!(entry.name, "Big Pickle");
    }

    #[test]
    fn parse_catalog_maps_providers_and_sorts() {
        let body: HashMap<String, ProviderRaw> = serde_json::from_value(serde_json::json!({
            "anthropic": { "models": { "claude-b": { "id": "claude-b" }, "claude-a": { "id": "claude-a" } } },
            "openai": { "models": { "gpt-z": { "id": "gpt-z", "name": "GPT Z" } } }
        }))
        .unwrap();
        let cat = parse_catalog(body, SystemTime::now());
        // Sorted by (provider, id): anthropic before openai, claude-a before claude-b.
        let pairs: Vec<(String, String)> = cat
            .models
            .iter()
            .map(|m| (m.provider.clone(), m.id.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("anthropic".to_string(), "claude-a".to_string()),
                ("anthropic".to_string(), "claude-b".to_string()),
                ("openai".to_string(), "gpt-z".to_string()),
            ]
        );
    }

    #[test]
    fn opencode_platform_mapping_includes_its_native_providers() {
        // The cross-checked platform must reach the providers that host its
        // zen catalog, or big-pickle (provider `opencode`) is invisible.
        let providers = providers_for_cli("opencode");
        assert!(providers.contains(&"opencode"));
        assert!(providers.contains(&"opencode-go"));
        // Real models.dev slug, not the non-existent `amazon`.
        assert!(providers.contains(&"amazon-bedrock"));
        assert!(!providers.contains(&"amazon"));
    }

    #[test]
    fn unknown_cli_has_no_providers() {
        assert!(providers_for_cli("no-such-cli").is_empty());
    }
}
