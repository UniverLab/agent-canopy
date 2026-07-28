//! TTL-cached catalog listing per source, so `skill_list` doesn't hit the
//! network on every call. Cache files live under
//! `<store_dir>/.catalog/<source-slug>.toml`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::source::{CatalogEntry, GitSource};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedCatalog {
    last_checked: DateTime<Utc>,
    entries: Vec<CatalogEntry>,
}

/// Return a source's catalog, using the TTL cache when fresh. Past TTL,
/// re-fetches from the source; on network failure, serves the last cached
/// catalog (if any) and logs a WARN rather than failing.
pub fn catalog_for(
    store_dir: &Path,
    source: &GitSource,
    ttl: chrono::Duration,
    now: DateTime<Utc>,
) -> Vec<CatalogEntry> {
    let path = cache_path(store_dir, source);
    let cached = load_cache(&path);

    if let Some(cached) = &cached {
        if now - cached.last_checked < ttl {
            return cached.entries.clone();
        }
    }

    match source.fetch_catalog() {
        Ok((entries, _commit_hash)) => {
            let fresh = CachedCatalog {
                last_checked: now,
                entries: entries.clone(),
            };
            if let Err(e) = save_cache(&path, &fresh) {
                tracing::warn!(
                    "dynamic_skills: failed to write catalog cache for {}: {e}",
                    source.url
                );
            }
            entries
        }
        Err(e) => {
            tracing::warn!(
                "dynamic_skills: catalog check failed for {} ({e}); serving cached catalog",
                source.url
            );
            cached.map(|c| c.entries).unwrap_or_default()
        }
    }
}

fn cache_path(store_dir: &Path, source: &GitSource) -> PathBuf {
    store_dir
        .join(".catalog")
        .join(format!("{}.toml", slug(source)))
}

fn slug(source: &GitSource) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(source.url.as_bytes());
    hasher.update(b"@");
    hasher.update(source.git_ref.as_deref().unwrap_or("HEAD").as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn load_cache(path: &Path) -> Option<CachedCatalog> {
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

fn save_cache(path: &Path, cache: &CachedCatalog) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(cache)?;
    std::fs::write(path, content)?;
    Ok(())
}
