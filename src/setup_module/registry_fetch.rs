use crate::setup_module::models::{
    is_binary_available, resolve_config_path, CanonicalServers, Platform, RegistryRaw,
};
use crate::setup_module::PlatformWithCli;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Global override for local registry development mode.
/// When set, canopy reads registry files from this local directory
/// instead of fetching from the remote GitHub repository.
static LOCAL_REGISTRY_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Set the local registry path for development mode.
/// This should be called early (e.g. in main.rs CLI parsing) before any registry fetch.
pub fn set_local_registry(path: PathBuf) {
    let _ = LOCAL_REGISTRY_PATH.set(path);
}

/// Get the currently configured local registry path, if any.
fn get_local_registry() -> Option<&'static PathBuf> {
    LOCAL_REGISTRY_PATH.get()
}

/// Lightweight index for the per-platform registry (v6).
#[derive(Deserialize)]
struct RegistryIndex {
    #[allow(dead_code)]
    version: u32,
    platforms: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    name: String,
    binary: String,
}

/// Legacy index (v5, JSON).
#[derive(Deserialize)]
struct LegacyRegistryIndex {
    #[allow(dead_code)]
    version: u32,
    platforms: Vec<IndexEntry>,
}

const REGISTRY_BASE_URL: &str = "https://raw.githubusercontent.com/UniverLab/canopy-registry/main/";

const REGISTRY_LEGACY_URL: &str =
    "https://raw.githubusercontent.com/UniverLab/canopy-registry/main/platforms.json";

/// How often to refresh the registry in the background (24 hours).
const REGISTRY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Fetch the platform registry (public for use by config commands).
#[allow(dead_code)]
pub fn fetch_registry_raw() -> Result<RegistryRaw> {
    fetch_registry()
}

pub(crate) fn fetch_registry() -> Result<RegistryRaw> {
    // Development mode: read from local filesystem if configured
    if let Some(local_path) = get_local_registry() {
        if let Some(reg) = try_fetch_local(local_path) {
            return Ok(reg);
        }
        anyhow::bail!(
            "Local registry not found or invalid at: {}",
            local_path.display()
        );
    }

    let client = reqwest::blocking::Client::new();

    // Try v6 (TOML) first
    if let Some(reg) = try_fetch_v6(&client) {
        return Ok(reg);
    }

    // Try v5 (JSON per-platform)
    if let Some(reg) = try_fetch_v5(&client) {
        return Ok(reg);
    }

    // Fallback: legacy monolithic platforms.json (v4)
    let response = client
        .get(REGISTRY_LEGACY_URL)
        .header("User-Agent", "canopy")
        .send()
        .context("Failed to connect to platform registry")?;

    if !response.status().is_success() {
        anyhow::bail!("Registry returned HTTP {}", response.status());
    }

    #[derive(Deserialize)]
    struct LegacyRaw {
        platforms: Vec<Platform>,
    }

    let legacy: LegacyRaw = response.json().context("Invalid registry JSON")?;
    Ok(RegistryRaw {
        platforms: legacy.platforms,
        canonical_servers: CanonicalServers::default(),
    })
}

/// Try reading registry v6 from a local directory (development mode).
/// Expects the directory to contain: index.toml, servers.toml, platforms/*.toml
fn try_fetch_local(base: &Path) -> Option<RegistryRaw> {
    let index_path = base.join("index.toml");
    let index_text = std::fs::read_to_string(&index_path).ok()?;
    let index: RegistryIndex = toml::from_str(&index_text).ok()?;

    // Read canonical servers
    let servers_path = base.join("servers.toml");
    let canonical_servers: CanonicalServers = if servers_path.exists() {
        let text = std::fs::read_to_string(&servers_path).ok()?;
        toml::from_str(&text).unwrap_or_default()
    } else {
        CanonicalServers::default()
    };

    // Read platform files (only for installed binaries)
    let needed: Vec<&IndexEntry> = index
        .platforms
        .iter()
        .filter(|e| is_binary_available(&e.binary))
        .collect();

    let platforms_dir = base.join("platforms");
    let mut platforms = Vec::new();
    for entry in &needed {
        let file_path = platforms_dir.join(format!("{}.toml", entry.name));
        match std::fs::read_to_string(&file_path) {
            Ok(text) => match toml::from_str::<Platform>(&text) {
                Ok(p) => platforms.push(p),
                Err(e) => {
                    tracing::warn!("Failed to parse local platform '{}': {e}", entry.name);
                }
            },
            Err(e) => {
                tracing::warn!(
                    "Failed to read local platform file '{}': {e}",
                    file_path.display()
                );
            }
        }
    }

    if platforms.is_empty() {
        return None;
    }

    Some(RegistryRaw {
        platforms,
        canonical_servers,
    })
}

/// Try fetching registry v6 (TOML index + servers + platforms).
fn try_fetch_v6(client: &reqwest::blocking::Client) -> Option<RegistryRaw> {
    let index_resp = client
        .get(format!("{REGISTRY_BASE_URL}index.toml"))
        .header("User-Agent", "canopy")
        .send()
        .ok()?;

    if !index_resp.status().is_success() {
        return None;
    }

    let index_text = index_resp.text().ok()?;
    let index: RegistryIndex = toml::from_str(&index_text).ok()?;

    // Fetch canonical servers
    let servers_resp = client
        .get(format!("{REGISTRY_BASE_URL}servers.toml"))
        .header("User-Agent", "canopy")
        .send()
        .ok()?;

    let canonical_servers: CanonicalServers = if servers_resp.status().is_success() {
        let text = servers_resp.text().ok()?;
        toml::from_str(&text).unwrap_or_default()
    } else {
        CanonicalServers::default()
    };

    // Fetch platform files (only for installed binaries)
    let needed: Vec<&IndexEntry> = index
        .platforms
        .iter()
        .filter(|e| is_binary_available(&e.binary))
        .collect();

    let mut platforms = Vec::new();
    for entry in &needed {
        let url = format!("{REGISTRY_BASE_URL}platforms/{}.toml", entry.name);
        match client
            .get(&url)
            .header("User-Agent", "canopy")
            .send()
            .and_then(|r| r.text())
        {
            Ok(text) => match toml::from_str::<Platform>(&text) {
                Ok(p) => platforms.push(p),
                Err(e) => {
                    tracing::warn!("Failed to parse platform '{}': {e}", entry.name);
                }
            },
            Err(e) => {
                tracing::warn!("Failed to fetch platform '{}': {e}", entry.name);
            }
        }
    }

    if platforms.is_empty() {
        return None;
    }

    Some(RegistryRaw {
        platforms,
        canonical_servers,
    })
}

/// Try fetching registry v5 (JSON per-platform).
fn try_fetch_v5(client: &reqwest::blocking::Client) -> Option<RegistryRaw> {
    let resp = client
        .get(format!("{REGISTRY_BASE_URL}index.json"))
        .header("User-Agent", "canopy")
        .send()
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let index: LegacyRegistryIndex = resp.json().ok()?;

    let needed: Vec<&IndexEntry> = index
        .platforms
        .iter()
        .filter(|e| is_binary_available(&e.binary))
        .collect();

    let mut platforms = Vec::new();
    for entry in &needed {
        let url = format!("{REGISTRY_BASE_URL}platforms/{}.json", entry.name);
        match client
            .get(&url)
            .header("User-Agent", "canopy")
            .send()
            .and_then(|r| r.json::<Platform>())
        {
            Ok(p) => platforms.push(p),
            Err(e) => {
                tracing::warn!("Failed to fetch platform '{}': {e}", entry.name);
            }
        }
    }

    if platforms.is_empty() {
        return None;
    }

    Some(RegistryRaw {
        platforms,
        canonical_servers: CanonicalServers::default(),
    })
}

use crate::shared::banner;

pub(crate) fn print_banner() {
    banner::print_banner_with_gradient("Agent Hub — Setup Wizard");
}

pub fn maybe_refresh_registry() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let config_path = home.join(".canopy/config.toml");

    // Check if file exists and when it was last modified
    let last_modified = match std::fs::metadata(&config_path) {
        Ok(meta) => meta.modified().ok(),
        Err(_) => return false,
    };

    let needs_refresh = match last_modified {
        Some(time) => time.elapsed().unwrap_or_default() > REGISTRY_REFRESH_INTERVAL,
        None => true,
    };

    if !needs_refresh {
        return false;
    }

    // Fetch and update in background thread
    std::thread::spawn(move || {
        let _ = refresh_registry_inner(&home);
    });

    true
}

fn refresh_registry_inner(home: &Path) -> Result<()> {
    let registry = fetch_registry()?;

    let detected: Vec<&Platform> = registry
        .platforms
        .iter()
        .filter(|p| resolve_config_path(home, &p.config_path).exists())
        .collect();

    let platforms_with_cli: Vec<PlatformWithCli> = detected
        .iter()
        .map(|p| p.to_platform_with_cli())
        .filter(|p| p.cli.is_some())
        .collect();

    let cli_registry =
        crate::domain::cli_config::CliRegistry::detect_available(&platforms_with_cli);

    let canopy_dir = home.join(".canopy");
    let mut config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);

    // Merge: add newly detected CLIs that aren't already configured, and remove
    // CLIs that the registry no longer knows about AND whose binary is gone.
    // We deliberately preserve manually-configured CLIs even if `which` can't
    // find them right now (e.g. NVM binaries, custom installs) to avoid
    // silently deleting entries the user set up intentionally.
    let known_names: std::collections::HashSet<String> = platforms_with_cli
        .iter()
        .filter_map(|p| p.cli.as_ref().map(|c| c.name.clone()))
        .collect();

    // Remove CLIs only when the registry explicitly knows the platform AND
    // the binary is confirmed missing.
    config.clis.retain(|c| {
        // Not in registry at all → keep (manually added)
        if !known_names.contains(&c.name) {
            return true;
        }
        // In registry → keep only if binary is present
        c.is_available()
    });

    // Add newly detected CLIs that aren't already in config.
    let existing_names: std::collections::HashSet<String> =
        config.clis.iter().map(|c| c.name.clone()).collect();
    for cli in cli_registry.available_clis {
        if !existing_names.contains(&cli.name) {
            config.clis.push(cli);
        }
    }

    if !config.clis.is_empty() {
        let _ = config.save(&canopy_dir);
    }

    Ok(())
}
