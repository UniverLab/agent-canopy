use crate::application::ports::StateRepository;
use crate::db::Database;
use crate::domain::cli_config::CliConfig;
use crate::domain::db_paths::database_path;
use crate::domain::registry_baseline::RegistryBaseline;
use crate::setup_module::models::{resolve_config_path, CanonicalServers, Platform, RegistryRaw};
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
    #[allow(dead_code)]
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

    let platforms_dir = base.join("platforms");
    let mut platforms = Vec::new();
    for entry in &index.platforms {
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

    let mut platforms = Vec::new();
    for entry in &index.platforms {
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

    let mut platforms = Vec::new();
    for entry in &index.platforms {
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

    Some(RegistryRaw {
        platforms,
        canonical_servers: CanonicalServers::default(),
    })
}

use crate::shared::banner;

pub(crate) fn print_banner() {
    banner::print_banner_with_gradient("Agent Hub — Setup Wizard");
}

/// State-table key holding the RFC 3339 timestamp of the last *successful*
/// registry fetch. Deliberately not derived from `config.toml`'s mtime:
/// anything that writes the config (canopy adding a CLI, a user changing a
/// setting) used to reset that clock and could postpone a refresh
/// indefinitely on a config that's touched regularly.
const REGISTRY_LAST_REFRESH_STATE_KEY: &str = "registry_last_refresh_at";

pub fn maybe_refresh_registry() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let canopy_dir = home.join(".canopy");
    // Only ever refresh a config that already exists -- pre-setup, there's
    // nothing to reconcile.
    if !canopy_dir.join("config.toml").exists() {
        return false;
    }

    if !needs_refresh(&canopy_dir) {
        return false;
    }

    // Fetch and update in background thread
    std::thread::spawn(move || {
        let _ = refresh_registry_inner(&home);
    });

    true
}

/// Whether enough time has passed since the last *successful* refresh,
/// reading the clock from daemon state rather than any file's mtime. No
/// recorded refresh (fresh database, or a database that doesn't exist yet)
/// is treated the same as an overdue one.
fn needs_refresh(canopy_dir: &Path) -> bool {
    let db_path = database_path(canopy_dir);
    let last_refresh = db_path
        .exists()
        .then(|| Database::new_safe(&db_path, canopy_dir).ok())
        .flatten()
        .and_then(|db| db.get_state(REGISTRY_LAST_REFRESH_STATE_KEY).ok().flatten());

    let Some(last_refresh) = last_refresh else {
        return true;
    };

    match chrono::DateTime::parse_from_rfc3339(&last_refresh) {
        Ok(ts) => {
            let elapsed = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
            elapsed
                .to_std()
                .map(|d| d > REGISTRY_REFRESH_INTERVAL)
                .unwrap_or(true)
        }
        Err(_) => true,
    }
}

fn refresh_registry_inner(home: &Path) -> Result<()> {
    let registry = fetch_registry()?;
    apply_registry_refresh(home, &registry)
}

/// The testable heart of a registry refresh: given an already-fetched
/// registry, detect which CLIs apply to this machine, three-way merge their
/// registry-owned fields into the existing config, add newly detected CLIs,
/// remove ones whose binary vanished, persist the baseline sidecar, and
/// record the successful refresh time. Split out from [`refresh_registry_inner`]
/// so tests can drive it with an in-memory [`RegistryRaw`] instead of a real
/// fetch.
fn apply_registry_refresh(home: &Path, registry: &RegistryRaw) -> Result<()> {
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

    let registry_by_name: std::collections::HashMap<String, CliConfig> = cli_registry
        .available_clis
        .iter()
        .map(|c| (c.name.clone(), c.clone()))
        .collect();

    let baseline = RegistryBaseline::load(&canopy_dir);
    let is_first_refresh = baseline.is_none();

    // First-ever refresh: there's no baseline to tell a deliberate local
    // edit apart from an untouched value, so adopt registry truth outright
    // for every existing CLI -- but back up the config first. The set of
    // users with intentional hand edits to these fields is small and their
    // config is recoverable from this backup; the set stuck with broken
    // invocation flags is everyone, permanently, with no way to even know.
    if is_first_refresh {
        let config_path = canopy_dir.join("config.toml");
        if config_path.exists() {
            let backup_path = canopy_dir.join(format!(
                "config.toml.bak-{}",
                chrono::Utc::now().to_rfc3339()
            ));
            std::fs::copy(&config_path, &backup_path)
                .context("Failed to back up config.toml before first registry reconciliation")?;
        }
    }

    let mut changed_fields: Vec<String> = Vec::new();

    // Three-way merge, per field, not per CLI: a field still equal to what
    // the registry published at the previous baseline is user-untouched, so
    // the new registry value wins; a field that has drifted from the
    // baseline was deliberately edited, so it's left alone. A user who
    // customised `model_flag` must still receive a corrected `headless_mode`.
    for existing in config.clis.iter_mut() {
        if let Some(registry_cli) = registry_by_name.get(&existing.name) {
            let baseline_cli = baseline.as_ref().and_then(|b| b.get(&existing.name));
            let name = existing.name.clone();
            *existing = merge_cli_fields(
                existing,
                baseline_cli,
                registry_cli,
                &name,
                &mut changed_fields,
            );
        }
    }

    // Remove CLIs only when the registry explicitly knows the platform AND
    // the binary is confirmed missing. We deliberately preserve
    // manually-configured CLIs even if `which` can't find them right now
    // (e.g. NVM binaries, custom installs) to avoid silently deleting
    // entries the user set up intentionally.
    let known_names: std::collections::HashSet<String> = platforms_with_cli
        .iter()
        .filter_map(|p| p.cli.as_ref().map(|c| c.name.clone()))
        .collect();
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
    for cli in cli_registry.available_clis.iter().cloned() {
        if !existing_names.contains(&cli.name) {
            config.clis.push(cli);
        }
    }

    // This is the one moment where silence is the failure mode being fixed:
    // an operator who never asked for a value to change should see that it
    // did.
    if !changed_fields.is_empty() {
        tracing::warn!(
            "registry refresh updated {} field(s) not locally customised: {}",
            changed_fields.len(),
            changed_fields.join(", ")
        );
    }

    // Platform-level reconciliation: add / update / delete / unchanged.
    // Structural comparison (field-by-field) rather than textual ensures
    // reformatted content with identical semantics is not treated as a change.
    let baseline_platforms: std::collections::HashMap<String, &Platform> = baseline
        .as_ref()
        .map(|b| b.platforms.iter().map(|p| (p.name.clone(), p)).collect())
        .unwrap_or_default();

    let mut added_platforms: Vec<String> = Vec::new();
    let mut updated_platforms: Vec<String> = Vec::new();
    let mut unchanged_platforms: Vec<String> = Vec::new();
    let mut reconciled_platforms: Vec<Platform> = Vec::new();

    for platform in &registry.platforms {
        if !resolve_config_path(home, &platform.config_path).exists() {
            continue;
        }
        match baseline_platforms.get(&platform.name) {
            Some(baseline_platform) => {
                if platforms_differ(baseline_platform, platform) {
                    updated_platforms.push(platform.name.clone());
                } else {
                    unchanged_platforms.push(platform.name.clone());
                }
            }
            None => {
                added_platforms.push(platform.name.clone());
            }
        }
        reconciled_platforms.push((*platform).clone());
    }

    let new_platform_names: std::collections::HashSet<String> = reconciled_platforms
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let mut deleted_platforms: Vec<String> = Vec::new();
    if let Some(b) = baseline.as_ref() {
        for baseline_platform in &b.platforms {
            if !new_platform_names.contains(&baseline_platform.name) {
                // Only count as deleted if the platform's config was part of
                // the baseline's detected set; platforms never detected are
                // not reconciled and should not appear here, but since we
                // only ever store detected platforms, any missing name is a
                // genuine deletion.
                deleted_platforms.push(baseline_platform.name.clone());
            }
        }
    }

    if !added_platforms.is_empty() || !updated_platforms.is_empty() || !deleted_platforms.is_empty()
    {
        tracing::warn!(
            "registry refresh: {} added, {} updated, {} deleted, {} unchanged platform(s)",
            added_platforms.len(),
            updated_platforms.len(),
            deleted_platforms.len(),
            unchanged_platforms.len()
        );
        if !updated_platforms.is_empty() {
            tracing::warn!("  updated: {}", updated_platforms.join(", "));
        }
        if !added_platforms.is_empty() {
            tracing::warn!("  added: {}", added_platforms.join(", "));
        }
        if !deleted_platforms.is_empty() {
            tracing::warn!("  deleted: {}", deleted_platforms.join(", "));
        }
    }

    // Always persist: an empty `clis` here only happens when the registry
    // confirmed every remaining entry's binary is gone (the retain rule
    // above never touches a manually-added, registry-unknown entry), so
    // that's a real state worth writing, not a detection glitch to shield
    // the config from.
    config.save(&canopy_dir)?;

    RegistryBaseline {
        clis: cli_registry.available_clis,
        platforms: reconciled_platforms,
    }
    .save(&canopy_dir)?;

    let db = Database::new_safe(&database_path(&canopy_dir), &canopy_dir)?;
    db.set_state(
        REGISTRY_LAST_REFRESH_STATE_KEY,
        &chrono::Utc::now().to_rfc3339(),
    )?;

    Ok(())
}

/// Structural comparison of two Platform objects. Returns true when any
/// registry-controlled field differs. The `cli` field is excluded — it is
/// handled by the existing CLI three-way merge.
fn platforms_differ(old: &Platform, new: &Platform) -> bool {
    old.config_path != new.config_path
        || old.config_format != new.config_format
        || old.toml_array_format != new.toml_array_format
        || old.command_format != new.command_format
        || old.mcp_servers_key != new.mcp_servers_key
        || old.deprecated_keys != new.deprecated_keys
        || old.unsupported_keys != new.unsupported_keys
        || old.fields_mapping != new.fields_mapping
        || old.required_fields != new.required_fields
        || old.server_extras != new.server_extras
        || old.skills_dir != new.skills_dir
        || old.instruction_file != new.instruction_file
}

/// Merges one CLI's registry-owned fields into `local`, field by field: a
/// field still equal to what the registry published at the previous
/// `baseline` refresh is user-untouched, so the new `registry` value wins; a
/// field that has drifted from the baseline was deliberately edited, so it's
/// left alone. No `baseline` entry for this CLI (first-ever refresh, or a
/// config entry the baseline sidecar never recorded) means there is no way
/// to tell a customised field from an untouched one, so the registry value
/// is adopted outright.
///
/// Operates generically over whatever fields `registry` serializes rather
/// than naming them, so a newly added `CliConfig` field participates
/// automatically without this function changing -- the merge knows fields,
/// never which harness it's looking at.
fn merge_cli_fields(
    local: &CliConfig,
    baseline: Option<&CliConfig>,
    registry: &CliConfig,
    cli_name: &str,
    changed_fields: &mut Vec<String>,
) -> CliConfig {
    let local_val = serde_json::to_value(local).unwrap_or_default();
    let registry_val = serde_json::to_value(registry).unwrap_or_default();
    let baseline_obj = baseline
        .and_then(|b| serde_json::to_value(b).ok())
        .and_then(|v| v.as_object().cloned());

    let mut merged = local_val.as_object().cloned().unwrap_or_default();
    let registry_obj = registry_val.as_object().cloned().unwrap_or_default();

    for (key, registry_field) in &registry_obj {
        let local_field = merged.get(key).cloned().unwrap_or(serde_json::Value::Null);
        let user_untouched = match &baseline_obj {
            Some(b) => b.get(key).cloned().unwrap_or(serde_json::Value::Null) == local_field,
            None => true,
        };
        if user_untouched && &local_field != registry_field {
            changed_fields.push(format!("{cli_name}.{key}"));
            merged.insert(key.clone(), registry_field.clone());
        }
    }

    serde_json::from_value(serde_json::Value::Object(merged)).unwrap_or_else(|_| local.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::registry_baseline::REGISTRY_BASELINE_FILE_NAME;
    use tempfile::TempDir;

    /// A registry platform whose config file already exists under `home`
    /// (so it's "detected") and whose `cli.binary` resolves via PATH (so
    /// it's "available") -- `ls` is present on every machine these tests
    /// run on. Callers can override `cli_json` fields.
    fn detected_platform(home: &Path, name: &str, cli_json: serde_json::Value) -> Platform {
        let config_path = format!("{name}.marker");
        std::fs::write(home.join(&config_path), "").unwrap();
        Platform {
            name: name.to_string(),
            config_path,
            config_format: None,
            toml_array_format: false,
            command_format: "separate".to_string(),
            mcp_servers_key: vec![],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: Some(cli_json),
        }
    }

    fn registry_of(platforms: Vec<Platform>) -> RegistryRaw {
        RegistryRaw {
            platforms,
            canonical_servers: CanonicalServers::default(),
        }
    }

    fn write_config(canopy_dir: &Path, clis: Vec<CliConfig>) {
        let mut config = crate::domain::canopy_config::CanopyConfig::load(canopy_dir);
        config.clis = clis;
        config.save(canopy_dir).unwrap();
    }

    fn write_baseline(canopy_dir: &Path, clis: Vec<CliConfig>) {
        RegistryBaseline {
            clis,
            ..Default::default()
        }
        .save(canopy_dir)
        .unwrap();
    }

    #[test]
    fn field_never_touched_by_user_is_updated_on_refresh() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        let local = CliConfig {
            name: "testcli".to_string(),
            binary: "ls".to_string(),
            headless_mode: "--old-headless".to_string(),
            ..Default::default()
        };
        write_config(&canopy_dir, vec![local.clone()]);
        write_baseline(&canopy_dir, vec![local]);

        let registry = registry_of(vec![detected_platform(
            home.path(),
            "testcli",
            serde_json::json!({"binary": "ls", "headless_mode": "--new-headless"}),
        )]);

        apply_registry_refresh(home.path(), &registry).unwrap();

        let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
        let cli = config.get_cli("testcli").unwrap();
        assert_eq!(cli.headless_mode, "--new-headless");
    }

    #[test]
    fn field_customised_by_user_survives_refresh() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        // Baseline reflects what the registry said last time.
        let baseline_cli = CliConfig {
            name: "testcli".to_string(),
            binary: "ls".to_string(),
            model_flag: Some("--model-old".to_string()),
            headless_mode: "--old-headless".to_string(),
            ..Default::default()
        };
        // Local drifted from the baseline on model_flag (deliberate edit)
        // but never touched headless_mode.
        let local_cli = CliConfig {
            model_flag: Some("--model-custom".to_string()),
            ..baseline_cli.clone()
        };
        write_config(&canopy_dir, vec![local_cli]);
        write_baseline(&canopy_dir, vec![baseline_cli]);

        let registry = registry_of(vec![detected_platform(
            home.path(),
            "testcli",
            serde_json::json!({
                "binary": "ls",
                "model_flag": "--model-new",
                "headless_mode": "--new-headless"
            }),
        )]);

        apply_registry_refresh(home.path(), &registry).unwrap();

        let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
        let cli = config.get_cli("testcli").unwrap();
        // Customised field is untouched...
        assert_eq!(cli.model_flag.as_deref(), Some("--model-custom"));
        // ...but the field the user never touched still gets corrected.
        assert_eq!(cli.headless_mode, "--new-headless");
    }

    #[test]
    fn brand_new_cli_is_still_added() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");
        write_config(&canopy_dir, vec![]);
        write_baseline(&canopy_dir, vec![]);

        let registry = registry_of(vec![detected_platform(
            home.path(),
            "brandnew",
            serde_json::json!({"binary": "ls", "headless_mode": "--headless"}),
        )]);

        apply_registry_refresh(home.path(), &registry).unwrap();

        let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
        assert!(config.get_cli("brandnew").is_some());
    }

    #[test]
    fn cli_whose_binary_vanished_is_still_removed() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        let ghost = CliConfig {
            name: "ghost".to_string(),
            binary: "canopy-test-fixture-cli-missing-xyz".to_string(),
            ..Default::default()
        };
        write_config(&canopy_dir, vec![ghost.clone()]);
        write_baseline(&canopy_dir, vec![ghost]);

        // Registry still knows about "ghost" (its marker file is detected),
        // but its binary can never resolve.
        let registry = registry_of(vec![detected_platform(
            home.path(),
            "ghost",
            serde_json::json!({"binary": "canopy-test-fixture-cli-missing-xyz"}),
        )]);

        apply_registry_refresh(home.path(), &registry).unwrap();

        let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
        assert!(config.get_cli("ghost").is_none());
    }

    #[test]
    fn first_refresh_with_no_baseline_adopts_and_backs_up() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        let local = CliConfig {
            name: "testcli".to_string(),
            binary: "ls".to_string(),
            headless_mode: "--stale".to_string(),
            ..Default::default()
        };
        write_config(&canopy_dir, vec![local]);
        // No baseline written -- this is the very first refresh.
        assert!(RegistryBaseline::load(&canopy_dir).is_none());

        let registry = registry_of(vec![detected_platform(
            home.path(),
            "testcli",
            serde_json::json!({"binary": "ls", "headless_mode": "--corrected"}),
        )]);

        apply_registry_refresh(home.path(), &registry).unwrap();

        let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
        assert_eq!(
            config.get_cli("testcli").unwrap().headless_mode,
            "--corrected"
        );

        // A timestamped backup of the pre-refresh config exists and holds
        // the old value.
        let backups: Vec<_> = std::fs::read_dir(&canopy_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("config.toml.bak-"))
            .collect();
        assert_eq!(backups.len(), 1);
        let backup_content = std::fs::read_to_string(canopy_dir.join(&backups[0])).unwrap();
        assert!(backup_content.contains("--stale"));

        // A baseline now exists for the next refresh to compare against.
        assert!(RegistryBaseline::load(&canopy_dir).is_some());
    }

    #[test]
    fn failed_fetch_leaves_config_and_baseline_untouched() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        let local = CliConfig {
            name: "testcli".to_string(),
            binary: "ls".to_string(),
            headless_mode: "--untouched".to_string(),
            ..Default::default()
        };
        write_config(&canopy_dir, vec![local.clone()]);
        write_baseline(&canopy_dir, vec![local]);

        let config_before = std::fs::read_to_string(canopy_dir.join("config.toml")).unwrap();
        let baseline_before =
            std::fs::read_to_string(canopy_dir.join(REGISTRY_BASELINE_FILE_NAME)).unwrap();

        // Point local-registry mode at a directory with no registry files
        // so `fetch_registry` fails deterministically, with no network call.
        let empty_registry_dir = TempDir::new().unwrap();
        set_local_registry(empty_registry_dir.path().to_path_buf());

        let result = refresh_registry_inner(home.path());
        assert!(result.is_err());

        let config_after = std::fs::read_to_string(canopy_dir.join("config.toml")).unwrap();
        let baseline_after =
            std::fs::read_to_string(canopy_dir.join(REGISTRY_BASELINE_FILE_NAME)).unwrap();
        assert_eq!(config_before, config_after);
        assert_eq!(baseline_before, baseline_after);
    }

    #[test]
    fn needs_refresh_true_when_no_state_recorded() {
        let dir = TempDir::new().unwrap();
        assert!(needs_refresh(dir.path()));
    }

    #[test]
    fn needs_refresh_false_shortly_after_a_recorded_refresh() {
        let dir = TempDir::new().unwrap();
        let db = Database::new(&database_path(dir.path())).unwrap();
        db.set_state(
            REGISTRY_LAST_REFRESH_STATE_KEY,
            &chrono::Utc::now().to_rfc3339(),
        )
        .unwrap();

        assert!(!needs_refresh(dir.path()));
    }

    #[test]
    fn needs_refresh_true_when_recorded_refresh_is_stale() {
        let dir = TempDir::new().unwrap();
        let db = Database::new(&database_path(dir.path())).unwrap();
        let stale = chrono::Utc::now() - chrono::Duration::hours(25);
        db.set_state(REGISTRY_LAST_REFRESH_STATE_KEY, &stale.to_rfc3339())
            .unwrap();

        assert!(needs_refresh(dir.path()));
    }

    /// The defect this spec fixes: rewriting `config.toml` for an unrelated
    /// reason must not reset the refresh clock, because that clock no
    /// longer lives on the file's mtime.
    #[test]
    fn rewriting_config_toml_does_not_reset_the_refresh_clock() {
        let dir = TempDir::new().unwrap();
        let db = Database::new(&database_path(dir.path())).unwrap();
        let stale = chrono::Utc::now() - chrono::Duration::hours(25);
        db.set_state(REGISTRY_LAST_REFRESH_STATE_KEY, &stale.to_rfc3339())
            .unwrap();

        // Simulate an unrelated config write (e.g. a settings change),
        // which touches config.toml's mtime.
        write_config(
            dir.path(),
            vec![CliConfig {
                name: "unrelated".to_string(),
                ..Default::default()
            }],
        );

        // The stale state-table timestamp is what still governs eligibility.
        assert!(needs_refresh(dir.path()));
    }

    #[test]
    fn platform_fields_are_updated_on_refresh() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        let old_platform = Platform {
            name: "testcli".to_string(),
            config_path: "testcli.marker".to_string(),
            config_format: None,
            toml_array_format: false,
            command_format: "separate".to_string(),
            mcp_servers_key: vec!["old_key".to_string()],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: Some(serde_json::json!({"binary": "ls", "headless_mode": "--headless"})),
        };

        std::fs::write(home.path().join(&old_platform.config_path), "").unwrap();

        write_config(&canopy_dir, vec![]);
        let baseline = RegistryBaseline {
            clis: vec![],
            platforms: vec![old_platform],
        };
        baseline.save(&canopy_dir).unwrap();

        let new_platform = Platform {
            name: "testcli".to_string(),
            config_path: "testcli.marker".to_string(),
            config_format: None,
            toml_array_format: false,
            command_format: "merged".to_string(),
            mcp_servers_key: vec!["new_key".to_string()],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: Some(serde_json::json!({"binary": "ls", "headless_mode": "--headless"})),
        };
        let registry = registry_of(vec![new_platform]);

        apply_registry_refresh(home.path(), &registry).unwrap();

        let updated_baseline = RegistryBaseline::load(&canopy_dir).unwrap();
        let updated_platform = updated_baseline
            .platforms
            .iter()
            .find(|p| p.name == "testcli")
            .unwrap();
        assert_eq!(updated_platform.command_format, "merged");
        assert_eq!(
            updated_platform.mcp_servers_key,
            vec!["new_key".to_string()]
        );
    }

    #[test]
    fn platform_unchanged_when_registry_matches_baseline() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        let platform = Platform {
            name: "testcli".to_string(),
            config_path: "testcli.marker".to_string(),
            config_format: None,
            toml_array_format: false,
            command_format: "separate".to_string(),
            mcp_servers_key: vec!["key".to_string()],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: Some(serde_json::json!({"binary": "ls", "headless_mode": "--headless"})),
        };
        std::fs::write(home.path().join(&platform.config_path), "").unwrap();

        write_config(&canopy_dir, vec![]);
        RegistryBaseline {
            clis: vec![],
            platforms: vec![platform.clone()],
        }
        .save(&canopy_dir)
        .unwrap();

        let registry = registry_of(vec![platform]);
        apply_registry_refresh(home.path(), &registry).unwrap();

        let updated = RegistryBaseline::load(&canopy_dir).unwrap();
        assert_eq!(updated.platforms.len(), 1);
        assert_eq!(updated.platforms[0].command_format, "separate");
        assert_eq!(
            updated.platforms[0].mcp_servers_key,
            vec!["key".to_string()]
        );
    }

    #[test]
    fn platform_added_when_new_in_registry() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        write_config(&canopy_dir, vec![]);
        RegistryBaseline {
            clis: vec![],
            platforms: vec![],
        }
        .save(&canopy_dir)
        .unwrap();

        let platform = Platform {
            name: "brandnew".to_string(),
            config_path: "brandnew.marker".to_string(),
            config_format: None,
            toml_array_format: false,
            command_format: "separate".to_string(),
            mcp_servers_key: vec!["mcpServers".to_string()],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: Some(serde_json::json!({"binary": "ls", "headless_mode": "--headless"})),
        };
        std::fs::write(home.path().join(&platform.config_path), "").unwrap();

        let registry = registry_of(vec![platform]);
        apply_registry_refresh(home.path(), &registry).unwrap();

        let updated = RegistryBaseline::load(&canopy_dir).unwrap();
        assert_eq!(updated.platforms.len(), 1);
        assert_eq!(updated.platforms[0].name, "brandnew");
    }

    #[test]
    fn platform_deleted_when_removed_from_registry() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        let platform = Platform {
            name: "oldone".to_string(),
            config_path: "oldone.marker".to_string(),
            config_format: None,
            toml_array_format: false,
            command_format: "separate".to_string(),
            mcp_servers_key: vec![],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: Some(serde_json::json!({"binary": "ls"})),
        };
        std::fs::write(home.path().join(&platform.config_path), "").unwrap();

        write_config(&canopy_dir, vec![]);
        RegistryBaseline {
            clis: vec![],
            platforms: vec![platform],
        }
        .save(&canopy_dir)
        .unwrap();

        let registry = registry_of(vec![]);
        apply_registry_refresh(home.path(), &registry).unwrap();

        let updated = RegistryBaseline::load(&canopy_dir).unwrap();
        assert!(updated.platforms.is_empty());
    }

    #[test]
    fn refresh_is_idempotent_for_platforms() {
        let home = TempDir::new().unwrap();
        let canopy_dir = home.path().join(".canopy");

        let old_platform = Platform {
            name: "testcli".to_string(),
            config_path: "testcli.marker".to_string(),
            config_format: None,
            toml_array_format: false,
            command_format: "separate".to_string(),
            mcp_servers_key: vec!["old_key".to_string()],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: Some(serde_json::json!({"binary": "ls"})),
        };
        std::fs::write(home.path().join(&old_platform.config_path), "").unwrap();

        write_config(&canopy_dir, vec![]);
        RegistryBaseline {
            clis: vec![],
            platforms: vec![old_platform],
        }
        .save(&canopy_dir)
        .unwrap();

        let new_platform = Platform {
            name: "testcli".to_string(),
            config_path: "testcli.marker".to_string(),
            config_format: None,
            toml_array_format: false,
            command_format: "merged".to_string(),
            mcp_servers_key: vec!["new_key".to_string()],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: Some(serde_json::json!({"binary": "ls"})),
        };
        let registry = registry_of(vec![new_platform]);

        apply_registry_refresh(home.path(), &registry).unwrap();
        let after_first = RegistryBaseline::load(&canopy_dir).unwrap();
        let first_platform = after_first
            .platforms
            .iter()
            .find(|p| p.name == "testcli")
            .unwrap()
            .clone();

        apply_registry_refresh(home.path(), &registry).unwrap();
        let after_second = RegistryBaseline::load(&canopy_dir).unwrap();
        let second_platform = after_second
            .platforms
            .iter()
            .find(|p| p.name == "testcli")
            .unwrap();

        assert_eq!(
            first_platform.command_format,
            second_platform.command_format
        );
        assert_eq!(
            first_platform.mcp_servers_key,
            second_platform.mcp_servers_key
        );
        // Second refresh must be idempotent: still exactly one platform, unchanged.
        assert_eq!(after_second.platforms.len(), 1);
    }

    fn write_local_registry(dir: &Path, entries: &[(&str, &str, &str)]) {
        let mut index_toml = String::from("version = 6\nplatforms = [\n");
        for (name, binary, _platform_toml) in entries {
            index_toml.push_str(&format!(
                "  {{ name = \"{name}\", binary = \"{binary}\" }},\n"
            ));
        }
        index_toml.push_str("]\n");
        std::fs::write(dir.join("index.toml"), &index_toml).unwrap();

        let platforms_dir = dir.join("platforms");
        std::fs::create_dir_all(&platforms_dir).unwrap();
        for (name, _binary, platform_toml) in entries {
            std::fs::write(platforms_dir.join(format!("{name}.toml")), platform_toml).unwrap();
        }
    }

    const MINIMAL_PLATFORM_TOML: &str = r#"
name = "PLACEHOLDER"
config_path = "PLACEHOLDER.marker"
command_format = "separate"
mcp_servers_key = []
deprecated_keys = []
unsupported_keys = []
"#;

    fn platform_toml(name: &str) -> String {
        MINIMAL_PLATFORM_TOML.replace("PLACEHOLDER", name)
    }

    #[test]
    fn try_fetch_local_returns_all_platforms_regardless_of_binary_availability() {
        let dir = TempDir::new().unwrap();
        write_local_registry(
            dir.path(),
            &[
                ("available_plat", "ls", &platform_toml("available_plat")),
                (
                    "missing_plat",
                    "definitely_not_real_xyz",
                    &platform_toml("missing_plat"),
                ),
            ],
        );

        let reg = try_fetch_local(dir.path()).expect("should succeed");
        assert_eq!(reg.platforms.len(), 2);
        let names: Vec<&str> = reg.platforms.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"available_plat"));
        assert!(names.contains(&"missing_plat"));
    }

    #[test]
    fn try_fetch_local_returns_platforms_when_no_binaries_match() {
        let dir = TempDir::new().unwrap();
        write_local_registry(
            dir.path(),
            &[(
                "only_plat",
                "definitely_not_real_abc",
                &platform_toml("only_plat"),
            )],
        );

        let reg =
            try_fetch_local(dir.path()).expect("should succeed even with no binaries on PATH");
        assert_eq!(reg.platforms.len(), 1);
        assert_eq!(reg.platforms[0].name, "only_plat");
    }
}
