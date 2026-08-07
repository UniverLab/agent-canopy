//! `canopy models` — CLI-side control over the `agent_models` catalog cache.
//!
//! Mirrors the `agent_models` MCP tool's `refresh: true` path, but as a
//! standalone command: useful for a script, a cron job, or a user who wants
//! to warm the cache before an agent needs it rather than eating the fetch
//! latency on the first `agent_models` call.

use anyhow::Result;
use clap::Subcommand;

use crate::domain::canopy_config::CanopyConfig;
use crate::domain::models_db::CatalogSource;

#[derive(Subcommand)]
pub enum ModelsAction {
    /// Force-refresh the model catalog cache(s), bypassing the TTL.
    ///
    /// With no `--platform`, refreshes the shared models.dev catalog plus
    /// every configured platform's native model enumeration. With
    /// `--platform`, refreshes only that platform (its native enumeration if
    /// it has one, otherwise the shared models.dev catalog it relies on). A
    /// refresh that fails to reach its source leaves the previous cache in
    /// place rather than clearing it.
    Refresh {
        /// Only refresh this platform (e.g. "opencode"), instead of every
        /// configured platform plus the shared models.dev catalog.
        #[arg(long)]
        platform: Option<String>,
    },
}

pub async fn handle_models_action(action: ModelsAction) -> Result<()> {
    match action {
        ModelsAction::Refresh { platform } => handle_refresh(platform.as_deref()),
    }
}

fn handle_refresh(platform: Option<&str>) -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let config = CanopyConfig::load(&home.join(".canopy"));

    match platform {
        Some(name) => refresh_one_platform(&config, name),
        None => refresh_all(&config),
    }
}

fn refresh_one_platform(config: &CanopyConfig, name: &str) -> Result<()> {
    let Some(cli) = config.get_cli(name) else {
        return Err(anyhow::anyhow!(
            "Platform '{name}' is not configured in canopy. Configured platforms: {}",
            config.cli_names().join(", ")
        ));
    };

    match native_enumeration_cmd(cli) {
        Some((binary, args)) => refresh_native(name, binary, args, config),
        None => {
            refresh_catalog(config)?;
            println!(
                "'{name}' has no native model enumeration; refreshed the shared models.dev \
                 catalog instead."
            );
            Ok(())
        }
    }
}

fn refresh_all(config: &CanopyConfig) -> Result<()> {
    refresh_catalog(config)?;

    for cli in &config.clis {
        if let Some((binary, args)) = native_enumeration_cmd(cli) {
            refresh_native(&cli.name, binary, args, config)?;
        }
    }

    Ok(())
}

/// `(binary, args)` for a CLI's model-list command, `None` when it has none
/// configured — mirrors `handler::platform_enumeration_cmd`'s emptiness check.
fn native_enumeration_cmd(cli: &crate::domain::cli_config::CliConfig) -> Option<(&str, &str)> {
    let args = cli.models_list_cmd.as_deref()?.trim();
    if args.is_empty() || cli.binary.is_empty() {
        return None;
    }
    Some((cli.binary.as_str(), args))
}

fn refresh_catalog(config: &CanopyConfig) -> Result<()> {
    match crate::domain::models_db::load_catalog_with_source(true, config.models.catalog_ttl()) {
        Some(load) => {
            report_refresh(
                "models.dev catalog",
                load.source,
                load.catalog.fetched_at,
                load.catalog.models.len(),
            );
            Ok(())
        }
        None => Err(anyhow::anyhow!(
            "Could not refresh the models.dev catalog: models.dev is unreachable and no \
             cache exists to fall back to."
        )),
    }
}

fn refresh_native(name: &str, binary: &str, args: &str, config: &CanopyConfig) -> Result<()> {
    match crate::domain::models_db::load_native_models(
        name,
        binary,
        args,
        true,
        config.models.native_ttl(),
    ) {
        Some(load) => {
            report_refresh(
                &format!("'{name}' models"),
                load.source,
                load.catalog.fetched_at,
                load.catalog.ids.len(),
            );
            Ok(())
        }
        None => Err(anyhow::anyhow!(
            "Could not refresh '{name}' models: running its model-list command failed and \
             no cached enumeration exists."
        )),
    }
}

/// Print the outcome of one refresh. `Cache` can't actually occur here since
/// every caller passes `force_refresh: true`, but it's handled anyway so this
/// stays correct if that ever changes.
fn report_refresh(
    label: &str,
    source: CatalogSource,
    fetched_at: std::time::SystemTime,
    count: usize,
) {
    let timestamp = chrono::DateTime::<chrono::Utc>::from(fetched_at).to_rfc3339();
    match source {
        CatalogSource::Live => println!("{label}: refreshed ({count} models)."),
        CatalogSource::Cache => println!("{label}: {count} models (already fresh, from cache)."),
        CatalogSource::Stale => println!(
            "{label}: refresh failed (source unreachable) — kept the previous cache from \
             {timestamp} ({count} models)."
        ),
    }
}
