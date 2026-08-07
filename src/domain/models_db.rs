//! Cached model catalog from <https://models.dev>.
//!
//! Provides a flat list of AI model entries with provider metadata,
//! cached locally under `~/.canopy/cache/` for fast lookup. The catalog can
//! be filtered by CLI name so the new-agent dialog only shows relevant
//! models.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default TTL for the models.dev catalog cache, used when nothing in
/// `config.toml` overrides it (see `ModelsConfig::catalog_ttl_minutes`).
/// A full models.dev fetch is a large remote call, so it defaults to a full
/// day rather than refreshing on every miss.
pub const DEFAULT_CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Default TTL for a platform's native CLI model enumeration, used when
/// nothing in `config.toml` overrides it (see
/// `ModelsConfig::native_ttl_minutes`). Deliberately much shorter than
/// [`DEFAULT_CATALOG_TTL`]: running a local CLI's own `models` subcommand is
/// cheap (no large remote fetch), and its answer changes the moment the user
/// authenticates with a new provider through that CLI — a stale native cache
/// is far more likely to hide a model the user just unlocked than the
/// models.dev catalog is to miss a same-day release.
pub const DEFAULT_NATIVE_TTL: Duration = Duration::from_secs(60 * 60);

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
//
// Both the models.dev catalog and the per-platform native enumeration are
// pure caches canopy repopulates on a miss — never hand-edited — so they
// live as JSON under a directory that names them: `~/.canopy/cache/`,
// alongside (not mixed into) the downloaded embedding models under
// `~/.canopy/models/`. A `models/catalog/` split was considered instead,
// but `~/.canopy/models/` is already the fastembed download root (1.1 GB,
// out of scope for this layout — see `rag::model_acquisition`), so nesting
// catalog caches there would mean either touching that tree or leaving a
// `models/embeddings/` vs `models/catalog/` split where only one side
// matches what's actually on disk. A sibling `cache/` tree keeps the
// catalog caches named for what they are without relocating the embeddings.

/// Directory name for cache files that live under `~/.canopy/` but are
/// namespaced away from the top level and from `~/.canopy/models/`.
const CACHE_DIR: &str = "cache";

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".canopy")
            .join(CACHE_DIR)
            .join("models_catalog.json")
    })
}

/// One-time migration of the legacy flat cache files (`models_cache.json`,
/// `models_native_<cli>.json`) that used to live directly under
/// `~/.canopy` into `~/.canopy/cache/`.
///
/// Safe to call on every startup: each file migrates independently and is a
/// no-op once its new-layout counterpart already exists. These are pure
/// JSON caches canopy repopulates on any miss (see the module-level cache
/// path comment), so — unlike `usage.toml` — no format conversion is
/// needed: a same-filesystem `rename` is atomic, so a crash mid-migration
/// leaves the file readable at exactly one of the two paths, never neither.
/// Never touches `~/.canopy/models/` (the embedding model download cache).
///
/// Once a legacy path's new-layout counterpart exists, `move_if_absent`
/// defers deleting the legacy leftover while `other_instance_may_be_running`
/// is `true` (see its doc comment) — a still-running old binary's daemon
/// has been observed recreating a legacy cache file after this migration
/// already moved it once.
pub fn migrate_legacy_caches(canopy_dir: &Path, other_instance_may_be_running: bool) {
    let cache_dir = canopy_dir.join(CACHE_DIR);

    move_if_absent(
        &canopy_dir.join("models_cache.json"),
        &cache_dir.join("models_catalog.json"),
        &cache_dir,
        other_instance_may_be_running,
    );

    let Ok(entries) = std::fs::read_dir(canopy_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_legacy_native_cache = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with("models_native_") && name.ends_with(".json"));
        if !is_legacy_native_cache {
            continue;
        }
        // Unwrap is safe: `is_legacy_native_cache` only matches when
        // `file_name()` returned `Some`.
        let file_name = path.file_name().unwrap();
        move_if_absent(
            &path,
            &cache_dir.join(file_name),
            &cache_dir,
            other_instance_may_be_running,
        );
    }
}

/// Move `old_path` to `new_path` (creating `parent` first) unless `new_path`
/// already exists, in which case any leftover `old_path` — e.g. from a crash
/// between a prior migration's rename and cleanup, or an older binary's
/// still-running daemon recreating the legacy cache file after migration —
/// is removed instead, but only while `other_instance_may_be_running` is
/// `false`.
///
/// The caller (`ensure_data_dir` in `main.rs`) computes
/// `other_instance_may_be_running` once via
/// `daemon::process::other_instance_may_be_running` before calling in,
/// rather than this module reaching for daemon detection itself: this
/// `domain` module is also compiled standalone (see
/// `examples/rag_search.rs`) without the `daemon` module available. As with
/// `usage_stats::migrate_legacy_json`, "an old reader of the legacy path
/// may still exist" is assumed by default: this project rebuilds and
/// re-runs from source while the previously installed binary's daemon and
/// TUI are still live and writing to the paths this migration moves. The
/// deferred removal is retried on every later call (i.e. every startup)
/// once the caller reports no such process.
fn move_if_absent(
    old_path: &Path,
    new_path: &Path,
    parent: &Path,
    other_instance_may_be_running: bool,
) {
    if new_path.exists() {
        if !other_instance_may_be_running {
            let _ = std::fs::remove_file(old_path);
        }
        return;
    }
    if !old_path.exists() {
        return;
    }
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = std::fs::rename(old_path, new_path);
}

// ── Public API ──────────────────────────────────────────────────────

/// Load the catalog and report where it came from (`live`/`cache`/`stale`).
///
/// Policy:
/// - `force_refresh == false`: a fresh cache (within `ttl`) is served as
///   [`CatalogSource::Cache`] with **no network call** — the hot path. A
///   stale or missing cache triggers a fetch ([`CatalogSource::Live`]); if the
///   fetch fails but a cache exists, it is served as [`CatalogSource::Stale`]
///   rather than failing hard.
/// - `force_refresh == true`: always fetch. On success the result is
///   [`CatalogSource::Live`] (and newly published models — e.g. opencode's
///   `big-pickle` — appear immediately); on failure any existing cache is
///   served as [`CatalogSource::Stale`].
///
/// `ttl` is caller-supplied (see `ModelsConfig::catalog_ttl`) rather than a
/// compiled constant, so it can be configured in `config.toml`.
///
/// Returns `None` only when there is neither a usable cache nor a reachable
/// models.dev.
pub fn load_catalog_with_source(force_refresh: bool, ttl: Duration) -> Option<CatalogLoad> {
    resolve_catalog(force_refresh, ttl, load_from_cache, fetch_and_cache)
}

/// TTL-and-provenance core of [`load_catalog_with_source`], with the cache read
/// and the network fetch injected so the staleness policy is testable without
/// touching disk or the network.
fn resolve_catalog(
    force_refresh: bool,
    ttl: Duration,
    load_cache: impl FnOnce() -> Option<ModelCatalog>,
    fetch: impl FnOnce() -> Option<ModelCatalog>,
) -> Option<CatalogLoad> {
    let cached = load_cache();

    if !force_refresh {
        if let Some(catalog) = cached.as_ref().filter(|c| is_fresh(c, ttl)).cloned() {
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

/// Whether a cached catalog is still within `ttl`. A timestamp in the future
/// (clock skew) makes `elapsed()` error; treat that as stale so a bad clock
/// forces a refresh rather than pinning a possibly-wrong cache forever.
fn is_fresh(catalog: &ModelCatalog, ttl: Duration) -> bool {
    fetched_within_ttl(catalog.fetched_at, ttl)
}

/// Shared TTL check used by both the models.dev catalog and the per-platform
/// native enumeration. A future timestamp (clock skew) counts as stale.
fn fetched_within_ttl(fetched_at: SystemTime, ttl: Duration) -> bool {
    fetched_at.elapsed().unwrap_or(ttl) < ttl
}

/// Load the catalog without ever blocking on the network.
///
/// Returns whatever cache exists (even stale) immediately; when the cache is
/// stale or missing, a background thread refreshes it for the next caller.
/// Use this from interactive paths (TUI dialogs) where a synchronous fetch
/// would freeze the UI for up to the request timeout.
pub fn load_catalog_nonblocking(ttl: Duration) -> Option<ModelCatalog> {
    let cached = load_from_cache();
    let fresh = cached
        .as_ref()
        .is_some_and(|c| fetched_within_ttl(c.fetched_at, ttl));

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

// ── Platform-native model enumeration ───────────────────────────────

/// A platform's own passable model ids, captured from a CLI enumeration
/// (e.g. `opencode models`), plus when they were captured. Each id is the
/// literal string that platform's model flag accepts — for a universal gateway
/// that is the `provider/model` form (`opencode/big-pickle`), which models.dev
/// does not carry. Cached like [`ModelCatalog`] so the CLI is only run when the
/// cache is stale, missing, or force-refreshed — never on the hot path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeCatalog {
    /// Passable model ids, in the order the CLI emitted them.
    pub ids: Vec<String>,
    #[serde(with = "timestamp_serde")]
    pub fetched_at: SystemTime,
}

/// A loaded native enumeration together with its provenance.
#[derive(Debug, Clone)]
pub struct NativeLoad {
    pub catalog: NativeCatalog,
    pub source: CatalogSource,
}

/// Load a platform's native model enumeration and report where it came from.
///
/// Mirrors [`load_catalog_with_source`]'s staleness policy: a fresh cache is
/// served with no CLI call ([`CatalogSource::Cache`]); a stale/missing cache
/// (or `force_refresh`) runs `<binary> <args>` ([`CatalogSource::Live`]); a
/// failed run falls back to any existing cache as [`CatalogSource::Stale`]
/// rather than failing hard. `ttl` is caller-supplied (see
/// `ModelsConfig::native_ttl`) rather than a compiled constant. Returns `None`
/// only when there is neither a usable cache nor a successful enumeration.
pub fn load_native_models(
    cli: &str,
    binary: &str,
    args: &str,
    force_refresh: bool,
    ttl: Duration,
) -> Option<NativeLoad> {
    let cli = cli.to_string();
    let cache_key = cli.clone();
    resolve_native(
        force_refresh,
        ttl,
        || load_native_from_cache(&cache_key),
        || {
            let ids = run_model_enumeration(binary, args)?;
            let catalog = NativeCatalog {
                ids,
                fetched_at: SystemTime::now(),
            };
            save_native_to_cache(&cli, &catalog);
            Some(catalog)
        },
    )
}

/// TTL-and-provenance core of [`load_native_models`], with the cache read and
/// the enumeration injected so the policy — and the emitted id form — is
/// testable without touching disk or spawning a process.
fn resolve_native(
    force_refresh: bool,
    ttl: Duration,
    load_cache: impl FnOnce() -> Option<NativeCatalog>,
    enumerate: impl FnOnce() -> Option<NativeCatalog>,
) -> Option<NativeLoad> {
    let cached = load_cache();

    if !force_refresh {
        if let Some(catalog) = cached
            .as_ref()
            .filter(|c| fetched_within_ttl(c.fetched_at, ttl))
            .cloned()
        {
            return Some(NativeLoad {
                catalog,
                source: CatalogSource::Cache,
            });
        }
    }

    match enumerate() {
        Some(catalog) => Some(NativeLoad {
            catalog,
            source: CatalogSource::Live,
        }),
        None => cached.map(|catalog| NativeLoad {
            catalog,
            source: CatalogSource::Stale,
        }),
    }
}

/// Run `<binary> <args>` and parse its stdout into passable model ids. Returns
/// `None` if the process cannot be spawned or exits non-zero, so a failed
/// enumeration falls back to the cache rather than caching an empty list.
fn run_model_enumeration(binary: &str, args: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    let output = std::process::Command::new(binary)
        .args(&parts)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let ids = parse_native_ids(&String::from_utf8_lossy(&output.stdout));
    if ids.is_empty() {
        return None;
    }
    Some(ids)
}

/// Each non-empty, trimmed stdout line is one passable model id. Kept pure so
/// the enumeration output shape (verified against real `opencode models`) is
/// unit-testable.
fn parse_native_ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn native_cache_path(cli: &str) -> Option<PathBuf> {
    // Keep the file name to the platform name so a bogus `cli` can't escape the
    // cache directory; platform names are registry-controlled slugs.
    let safe: String = cli
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dirs::home_dir().map(|h| {
        h.join(".canopy")
            .join(CACHE_DIR)
            .join(format!("models_native_{safe}.json"))
    })
}

fn load_native_from_cache(cli: &str) -> Option<NativeCatalog> {
    let path = native_cache_path(cli)?;
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_native_to_cache(cli: &str, catalog: &NativeCatalog) {
    let Some(path) = native_cache_path(cli) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(catalog) {
        let _ = std::fs::write(path, json);
    }
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

    /// TTL used throughout these tests wherever the exact value doesn't
    /// matter beyond "the configured TTL" — both `resolve_catalog` and
    /// `resolve_native` take it as an explicit parameter now that it's
    /// configurable rather than a compiled constant.
    const TTL: Duration = DEFAULT_CATALOG_TTL;

    // ── migrate_legacy_caches ────────────────────────────────────────

    #[test]
    fn migrate_moves_catalog_and_native_caches_into_cache_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("models_cache.json"), r#"{"catalog":true}"#).unwrap();
        std::fs::write(
            dir.path().join("models_native_opencode.json"),
            r#"{"native":"opencode"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("models_native_claude.json"),
            r#"{"native":"claude"}"#,
        )
        .unwrap();

        migrate_legacy_caches(dir.path(), false);

        assert!(!dir.path().join("models_cache.json").exists());
        assert!(!dir.path().join("models_native_opencode.json").exists());
        assert!(!dir.path().join("models_native_claude.json").exists());

        let cache_dir = dir.path().join("cache");
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("models_catalog.json")).unwrap(),
            r#"{"catalog":true}"#
        );
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("models_native_opencode.json")).unwrap(),
            r#"{"native":"opencode"}"#
        );
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("models_native_claude.json")).unwrap(),
            r#"{"native":"claude"}"#
        );
    }

    #[test]
    fn migrate_never_touches_embedding_models_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::write(models_dir.join("some-embedding-file.onnx"), b"not a cache").unwrap();
        std::fs::write(dir.path().join("models_cache.json"), "{}").unwrap();

        migrate_legacy_caches(dir.path(), false);

        // The embedding download tree is untouched: still present, and
        // nothing from it leaked into the new cache dir.
        assert!(models_dir.join("some-embedding-file.onnx").exists());
        assert!(!dir.path().join("cache").join("models").exists());
    }

    #[test]
    fn migrate_is_idempotent_when_cache_already_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("models_catalog.json"), r#"{"fresh":true}"#).unwrap();
        // A leftover legacy file from a crash between a prior migration's
        // rename and its cleanup must not clobber the already-migrated copy.
        std::fs::write(dir.path().join("models_cache.json"), r#"{"stale":true}"#).unwrap();

        migrate_legacy_caches(dir.path(), false);

        assert!(!dir.path().join("models_cache.json").exists());
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("models_catalog.json")).unwrap(),
            r#"{"fresh":true}"#
        );
    }

    #[test]
    fn migrate_noop_on_fresh_install() {
        let dir = tempfile::TempDir::new().unwrap();
        migrate_legacy_caches(dir.path(), false);
        assert!(!dir.path().join("cache").exists());
    }

    // ── deferred deletion while a daemon may be running ─────────────────

    #[test]
    fn migrate_defers_deletion_of_leftover_legacy_cache_when_daemon_running() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("models_catalog.json"), r#"{"fresh":true}"#).unwrap();
        // As if an older binary's still-running daemon recreated the
        // legacy cache file after a prior migration already moved it once
        // (the observed real-world scenario this spec is about).
        std::fs::write(dir.path().join("models_cache.json"), r#"{"stale":true}"#).unwrap();

        migrate_legacy_caches(dir.path(), true);

        assert!(
            dir.path().join("models_cache.json").exists(),
            "must not delete a legacy cache file a live daemon might still be using"
        );
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("models_catalog.json")).unwrap(),
            r#"{"fresh":true}"#
        );
    }

    #[test]
    fn migrate_cleans_up_deferred_legacy_cache_once_daemon_stops() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("models_catalog.json"), r#"{"fresh":true}"#).unwrap();
        std::fs::write(dir.path().join("models_cache.json"), r#"{"stale":true}"#).unwrap();

        migrate_legacy_caches(dir.path(), true);
        assert!(
            dir.path().join("models_cache.json").exists(),
            "deletion should be deferred first"
        );

        migrate_legacy_caches(dir.path(), false);

        assert!(
            !dir.path().join("models_cache.json").exists(),
            "deferred cleanup must run on a later, quieter run"
        );
    }

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
            TTL,
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
            TTL,
            || {
                Some(catalog(
                    &[("anthropic", "old")],
                    TTL + Duration::from_secs(1),
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
            TTL,
            || {
                Some(catalog(
                    &[("anthropic", "old")],
                    TTL + Duration::from_secs(1),
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
        let load = resolve_catalog(false, TTL, || None, || None);
        assert!(load.is_none());
    }

    #[test]
    fn force_refresh_fetches_even_when_cache_is_fresh() {
        let fetched = Cell::new(false);
        let load = resolve_catalog(
            true,
            TTL,
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
            TTL,
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

        let cached = resolve_catalog(false, TTL, old_cache, fetch_with_pickle).unwrap();
        assert_eq!(cached.source, CatalogSource::Cache);
        assert!(
            !ids(&cached).contains(&"big-pickle".to_string()),
            "a fresh cache should not yet know about the new model"
        );

        // Forcing a refresh pulls it in from models.dev.
        let refreshed = resolve_catalog(true, TTL, old_cache, fetch_with_pickle).unwrap();
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

    // ── Platform-native enumeration ─────────────────────────────────

    fn native(ids: &[&str], age: Duration) -> NativeCatalog {
        NativeCatalog {
            ids: ids.iter().map(|s| (*s).to_string()).collect(),
            fetched_at: SystemTime::now() - age,
        }
    }

    #[test]
    fn parse_native_ids_keeps_one_prefixed_id_per_line() {
        // Shape verified against real `opencode models` output: one passable
        // `provider/model` id per line, blank lines ignored, whitespace trimmed.
        let out =
            "opencode/big-pickle\nopencode-go/glm-5.2\n\n  nvidia/meta/llama-3.3-70b-instruct  \n";
        assert_eq!(
            parse_native_ids(out),
            vec![
                "opencode/big-pickle".to_string(),
                "opencode-go/glm-5.2".to_string(),
                "nvidia/meta/llama-3.3-70b-instruct".to_string(),
            ]
        );
    }

    #[test]
    fn fresh_native_cache_is_served_without_enumerating() {
        let enumerated = Cell::new(false);
        let load = resolve_native(
            false,
            TTL,
            || Some(native(&["opencode/big-pickle"], Duration::from_secs(60))),
            || {
                enumerated.set(true);
                None
            },
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Cache);
        assert!(
            !enumerated.get(),
            "a fresh native cache must not spawn the CLI"
        );
        // opencode's form is the provider-prefixed id, verbatim from the CLI.
        assert_eq!(load.catalog.ids, vec!["opencode/big-pickle".to_string()]);
    }

    #[test]
    fn stale_native_cache_reenumerates_and_reports_live() {
        let load = resolve_native(
            false,
            TTL,
            || Some(native(&["opencode/old"], TTL + Duration::from_secs(1))),
            || {
                Some(native(
                    &["opencode/mimo-v2.5-free", "opencode/big-pickle"],
                    Duration::ZERO,
                ))
            },
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Live);
        // The zen models models.dev can't see now surface, in prefixed form.
        assert_eq!(
            load.catalog.ids,
            vec![
                "opencode/mimo-v2.5-free".to_string(),
                "opencode/big-pickle".to_string(),
            ]
        );
    }

    #[test]
    fn failed_native_enumeration_falls_back_to_stale_cache() {
        let load = resolve_native(
            false,
            TTL,
            || {
                Some(native(
                    &["opencode/big-pickle"],
                    TTL + Duration::from_secs(1),
                ))
            },
            || None,
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Stale);
        assert_eq!(load.catalog.ids, vec!["opencode/big-pickle".to_string()]);
    }

    #[test]
    fn no_native_cache_and_failed_enumeration_returns_none() {
        assert!(resolve_native(false, TTL, || None, || None).is_none());
    }

    #[test]
    fn force_refresh_reenumerates_even_when_native_cache_is_fresh() {
        let enumerated = Cell::new(false);
        let load = resolve_native(
            true,
            TTL,
            || Some(native(&["opencode/cached"], Duration::from_secs(1))),
            || {
                enumerated.set(true);
                Some(native(&["opencode/fresh"], Duration::ZERO))
            },
        )
        .unwrap();
        assert!(enumerated.get(), "force_refresh must always re-enumerate");
        assert_eq!(load.source, CatalogSource::Live);
        assert_eq!(load.catalog.ids, vec!["opencode/fresh".to_string()]);
    }

    // ── infer_size_hint ────────────────────────────────────────────────

    #[test]
    fn infer_size_hint_detects_billion_parameters() {
        assert_eq!(infer_size_hint("model-7b", None), Some("7B".to_string()));
        assert_eq!(
            infer_size_hint("llama-3.1-70b-instruct", None),
            Some("70B".to_string())
        );
        assert_eq!(
            infer_size_hint("model-0.5b", None),
            Some("0.5B".to_string())
        );
    }

    #[test]
    fn infer_size_hint_detects_million_parameters() {
        assert_eq!(
            infer_size_hint("model-125m", None),
            Some("125M".to_string())
        );
    }

    #[test]
    fn infer_size_hint_detects_named_sizes() {
        assert_eq!(
            infer_size_hint("bge-small-en", None),
            Some("small".to_string())
        );
        assert_eq!(
            infer_size_hint("bge-large-en", None),
            Some("large".to_string())
        );
        assert_eq!(
            infer_size_hint("bge-base-en", None),
            Some("base".to_string())
        );
    }

    #[test]
    fn infer_size_hint_checks_name_field_too() {
        assert_eq!(
            infer_size_hint("some-model", Some("Some 7B Model")),
            Some("7B".to_string())
        );
    }

    #[test]
    fn infer_size_hint_returns_none_for_no_match() {
        assert_eq!(infer_size_hint("claude-sonnet-4-6", None), None);
        assert_eq!(infer_size_hint("gpt-4o", None), None);
    }

    #[test]
    fn infer_size_hint_case_insensitive() {
        assert_eq!(
            infer_size_hint("model-SMALL-v2", None),
            Some("small".to_string())
        );
        assert_eq!(
            infer_size_hint("model-LARGE-v2", None),
            Some("large".to_string())
        );
    }

    // ── suggestions_for ────────────────────────────────────────────────

    fn test_catalog() -> ModelCatalog {
        ModelCatalog {
            models: vec![
                ModelEntry {
                    id: "claude-sonnet-4-6".to_string(),
                    name: "Claude Sonnet 4.6".to_string(),
                    provider: "anthropic".to_string(),
                    release_date: None,
                    size_hint: None,
                },
                ModelEntry {
                    id: "gpt-4o".to_string(),
                    name: "GPT-4o".to_string(),
                    provider: "openai".to_string(),
                    release_date: None,
                    size_hint: None,
                },
                ModelEntry {
                    id: "gemini-2.5-pro".to_string(),
                    name: "Gemini 2.5 Pro".to_string(),
                    provider: "google".to_string(),
                    release_date: None,
                    size_hint: None,
                },
                ModelEntry {
                    id: "claude-haiku-3.5".to_string(),
                    name: "Claude Haiku 3.5".to_string(),
                    provider: "anthropic".to_string(),
                    release_date: None,
                    size_hint: None,
                },
            ],
            fetched_at: SystemTime::now(),
        }
    }

    #[test]
    fn suggestions_for_filters_by_provider() {
        let catalog = test_catalog();
        let results = suggestions_for(&catalog, "claude", "");
        // claude only maps to anthropic
        assert!(results.iter().all(|m| m.provider == "anthropic"));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn suggestions_for_filters_by_query() {
        let catalog = test_catalog();
        let results = suggestions_for(&catalog, "claude", "sonnet");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "claude-sonnet-4-6");
    }

    #[test]
    fn suggestions_for_query_is_case_insensitive() {
        let catalog = test_catalog();
        let results = suggestions_for(&catalog, "claude", "SONNET");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn suggestions_for_empty_query_returns_all_matching_providers() {
        let catalog = test_catalog();
        let results = suggestions_for(&catalog, "claude", "");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn suggestions_for_unknown_cli_returns_all_models() {
        let catalog = test_catalog();
        // Unknown CLI → empty providers list → all models pass the filter
        let results = suggestions_for(&catalog, "unknown-cli", "");
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn suggestions_for_query_matches_name_field() {
        let catalog = test_catalog();
        let results = suggestions_for(&catalog, "claude", "Haiku");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "claude-haiku-3.5");
    }

    // ── CatalogSource::as_str ──────────────────────────────────────────

    #[test]
    fn catalog_source_as_str() {
        assert_eq!(CatalogSource::Live.as_str(), "live");
        assert_eq!(CatalogSource::Cache.as_str(), "cache");
        assert_eq!(CatalogSource::Stale.as_str(), "stale");
    }

    #[test]
    fn catalog_source_serde_round_trip() {
        let live = serde_json::to_string(&CatalogSource::Live).unwrap();
        assert_eq!(live, "\"live\"");
        let cache = serde_json::to_string(&CatalogSource::Cache).unwrap();
        assert_eq!(cache, "\"cache\"");
        let stale = serde_json::to_string(&CatalogSource::Stale).unwrap();
        assert_eq!(stale, "\"stale\"");
    }

    #[test]
    fn catalog_source_deserialize() {
        let live: CatalogSource = serde_json::from_str("\"live\"").unwrap();
        assert_eq!(live, CatalogSource::Live);
        let cache: CatalogSource = serde_json::from_str("\"cache\"").unwrap();
        assert_eq!(cache, CatalogSource::Cache);
        let stale: CatalogSource = serde_json::from_str("\"stale\"").unwrap();
        assert_eq!(stale, CatalogSource::Stale);
    }

    // ── ModelEntry serialization ───────────────────────────────────────

    #[test]
    fn model_entry_round_trip() {
        let entry = ModelEntry {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            provider: "test-provider".to_string(),
            release_date: Some("2025-01-01".to_string()),
            size_hint: Some("7B".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: ModelEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "test-model");
        assert_eq!(deserialized.name, "Test Model");
        assert_eq!(deserialized.provider, "test-provider");
        assert_eq!(deserialized.release_date.as_deref(), Some("2025-01-01"));
        assert_eq!(deserialized.size_hint.as_deref(), Some("7B"));
    }

    #[test]
    fn model_entry_optional_fields_none() {
        let entry = ModelEntry {
            id: "test".to_string(),
            name: "Test".to_string(),
            provider: "test".to_string(),
            release_date: None,
            size_hint: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: ModelEntry = serde_json::from_str(&json).unwrap();
        assert!(deserialized.release_date.is_none());
        assert!(deserialized.size_hint.is_none());
    }

    // ── providers_for_cli ──────────────────────────────────────────────

    #[test]
    fn providers_for_cli_all_known() {
        assert_eq!(providers_for_cli("claude"), &["anthropic"]);
        assert_eq!(providers_for_cli("codex"), &["openai"]);
        assert_eq!(providers_for_cli("mistral"), &["mistral"]);
        assert_eq!(providers_for_cli("copilot").len(), 6);
        assert_eq!(providers_for_cli("gemini"), &["google"]);
        assert_eq!(providers_for_cli("qwen"), &["alibaba"]);
        assert!(providers_for_cli("kiro").contains(&"anthropic"));
        assert!(providers_for_cli("kiro").contains(&"google"));
        assert!(providers_for_cli("opencode").contains(&"opencode"));
        assert!(providers_for_cli("opencode").contains(&"opencode-go"));
    }

    // ── parse_native_ids edge cases ────────────────────────────────────

    #[test]
    fn parse_native_ids_empty_output() {
        assert_eq!(parse_native_ids(""), Vec::<String>::new());
        assert_eq!(parse_native_ids("\n\n\n"), Vec::<String>::new());
    }

    #[test]
    fn parse_native_ids_single_line() {
        assert_eq!(
            parse_native_ids("opencode/big-pickle"),
            vec!["opencode/big-pickle".to_string()]
        );
    }

    #[test]
    fn parse_native_ids_trims_whitespace() {
        assert_eq!(
            parse_native_ids("  opencode/big-pickle  \n  opencode/mimo-v2.5-free  \n"),
            vec![
                "opencode/big-pickle".to_string(),
                "opencode/mimo-v2.5-free".to_string()
            ]
        );
    }

    // ── resolve_native edge cases ──────────────────────────────────────

    #[test]
    fn resolve_native_no_cache_and_successful_enumerate() {
        let load = resolve_native(
            false,
            TTL,
            || None,
            || Some(native(&["m1"], Duration::ZERO)),
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Live);
        assert_eq!(load.catalog.ids, vec!["m1".to_string()]);
    }

    #[test]
    fn force_refresh_enumeration_failure_returns_none_without_cache() {
        assert!(resolve_native(true, TTL, || None, || None).is_none());
    }

    #[test]
    fn force_refresh_with_cache_but_failed_enumeration_returns_stale() {
        let load = resolve_native(
            true,
            TTL,
            || Some(native(&["cached"], Duration::from_secs(1))),
            || None,
        )
        .unwrap();
        assert_eq!(load.source, CatalogSource::Stale);
        assert_eq!(load.catalog.ids, vec!["cached".to_string()]);
    }
}
