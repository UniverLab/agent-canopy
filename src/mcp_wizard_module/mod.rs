//! Interactive MCP management wizard — `canopy mcp`.
//!
//! Provides three operations:
//!  - **Sync**: scan all detected platforms and replicate MCPs found in any of
//!    them across every other platform (format-converted).
//!  - **Add**: prompt for Name / URL / Type and write the entry to every
//!    detected platform simultaneously.
//!  - **Remove**: show a unified server list and remove the chosen entry from
//!    every platform it appears in.

use anyhow::{Context, Result};
use inquire::{Select, Text};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::setup_module::{self as setup, Platform};

// ── Public entry point ─────────────────────────────────────────────────────

type PlatformConfigs = BTreeMap<String, BTreeMap<String, serde_json::Value>>;
type UnifiedServers<'a> = BTreeMap<String, (serde_json::Value, &'a str)>;
type MissingServers = Vec<(String, String)>;

#[derive(Clone, Copy)]
enum WizardAction {
    Sync,
    Add,
    Remove,
}

struct AddServerInput {
    name: String,
    url: String,
    server_type: String,
}

/// Run the interactive `canopy mcp` wizard.
pub fn run_mcp_wizard() -> Result<()> {
    let home = dirs::home_dir().context("No home directory")?;

    clear_screen()?;
    print_mcp_banner();

    let registry = fetch_registry()?;
    let detected = detect_available_platforms(&registry.platforms);

    if detected.is_empty() {
        print_no_supported_platforms(&registry.platforms);
        return Ok(());
    }

    print_detected_platforms(&detected);
    let pre_configs = collect_all_platform_configs(&home, &detected);
    print_mcp_table(&detected, &pre_configs);

    let Some(action) = prompt_wizard_action()? else {
        return Ok(());
    };

    match action {
        WizardAction::Sync => run_sync(&home, &detected),
        WizardAction::Add => run_add(&home, &detected),
        WizardAction::Remove => run_remove(&home, &detected),
    }
}

// ── Sync ───────────────────────────────────────────────────────────────────

/// Collect all MCP servers from every platform then replicate each missing one
/// to every platform that does not yet have it.
fn run_sync(home: &Path, detected: &[&Platform]) -> Result<()> {
    print_sync_header();

    let all_configs = collect_all_platform_configs(home, detected);
    let unified = build_unified_servers(&all_configs);
    if unified.is_empty() {
        println!("  \x1b[33m⚠\x1b[0m  No MCP servers found in any platform config.");
        return Ok(());
    }

    println!();
    print_mcp_table(detected, &all_configs);

    let missing = collect_missing_servers(detected, &all_configs, &unified);
    if missing.is_empty() {
        println!("  \x1b[32m✓\x1b[0m All platforms already in sync.");
        return Ok(());
    }

    let missing_count = missing.len();
    if !confirm(&format!(
        "\x1b[33m{missing_count}\x1b[0m server(s) to replicate. Proceed?"
    ))? {
        println!("  Cancelled.");
        return Ok(());
    }

    let (applied, errors) = apply_sync_to_platforms(home, detected, &all_configs, &unified);
    print_sync_summary(applied, errors);

    println!();
    show_updated_table(home, detected);
    Ok(())
}

fn print_sync_header() {
    println!();
    println!("  \x1b[1mGlobal MCP Sync\x1b[0m");
    println!("  ─────────────────────────────────────────────");
    println!("  Scanning platform configs…");
}

fn build_unified_servers(all_configs: &PlatformConfigs) -> UnifiedServers<'_> {
    let mut unified = BTreeMap::new();

    for (platform, servers) in all_configs {
        for (name, config) in servers {
            unified
                .entry(name.clone())
                .or_insert_with(|| (config.clone(), platform.as_str()));
        }
    }

    unified
}

fn collect_missing_servers(
    detected: &[&Platform],
    all_configs: &PlatformConfigs,
    unified: &UnifiedServers<'_>,
) -> MissingServers {
    let mut missing = Vec::new();

    for platform in detected {
        let existing = existing_server_names(all_configs, &platform.name);
        for name in unified.keys() {
            if !existing.contains(name) {
                missing.push((platform.name.clone(), name.clone()));
            }
        }
    }

    missing
}

fn apply_sync_to_platforms(
    home: &Path,
    detected: &[&Platform],
    all_configs: &PlatformConfigs,
    unified: &UnifiedServers<'_>,
) -> (usize, usize) {
    let mut applied = 0usize;
    let mut errors = 0usize;

    for platform in detected {
        let config_path = resolve_platform_config_path(home, platform);
        let existing = existing_server_names(all_configs, &platform.name);

        for (server_name, (config, _)) in unified {
            if existing.contains(server_name) {
                continue;
            }

            let adapted = setup::adapt_config(config, platform, server_name);
            match apply_server_to_platform(platform, &config_path, server_name, &adapted) {
                Ok(_) => applied += 1,
                Err(error) => {
                    eprintln!(
                        "    \x1b[31m✗\x1b[0m {}/{}: {}",
                        platform.name, server_name, error
                    );
                    errors += 1;
                }
            }
        }
    }

    (applied, errors)
}

fn print_sync_summary(applied: usize, errors: usize) {
    println!();
    if errors == 0 {
        println!("  \x1b[32m✓\x1b[0m Sync complete — {applied} server(s) replicated.");
        return;
    }

    println!("  \x1b[33m⚠\x1b[0m Sync partial — {applied} synced, {errors} failed.");
}

// ── Add ────────────────────────────────────────────────────────────────────

/// Interactively collect Name / URL / Type and inject into every detected platform.
fn run_add(home: &Path, detected: &[&Platform]) -> Result<()> {
    println!();
    println!("  \x1b[1mAdd MCP Server\x1b[0m");
    println!("  ─────────────────────────────────────────────");

    let Some(input) = prompt_add_server_input()? else {
        return Ok(());
    };

    let config = build_server_config(&input.url, &input.server_type);

    println!();
    println!(
        "  Installing \x1b[1m{}\x1b[0m ({}) on all platforms…",
        input.name, input.server_type
    );

    let (ok, fail) = add_server_to_platforms(home, detected, &input.name, &config);

    println!();
    if fail == 0 {
        println!(
            "  \x1b[32m✓\x1b[0m '{}' added to {ok} platform(s).",
            input.name
        );
    } else {
        println!(
            "  \x1b[33m⚠\x1b[0m '{}' added to {ok}, failed on {fail} platform(s).",
            input.name
        );
    }

    println!();
    show_updated_table(home, detected);
    Ok(())
}

fn prompt_add_server_input() -> Result<Option<AddServerInput>> {
    let name = prompt_trimmed_text("Server name (e.g. \"github\"):")?;
    if !validate_required_input(&name, "Server name is required.") {
        return Ok(None);
    }

    let url = prompt_trimmed_text("Server URL (e.g. \"https://example.com/mcp\"):")?;
    let server_type = prompt_server_type()?;

    Ok(Some(AddServerInput {
        name,
        url,
        server_type,
    }))
}

fn build_server_config(url: &str, server_type: &str) -> serde_json::Value {
    serde_json::json!({
        "type": server_type,
        "url": url,
    })
}

fn add_server_to_platforms(
    home: &Path,
    detected: &[&Platform],
    name: &str,
    config: &serde_json::Value,
) -> (usize, usize) {
    let mut ok = 0usize;
    let mut fail = 0usize;

    for platform in detected {
        let config_path = resolve_platform_config_path(home, platform);
        let adapted = setup::adapt_config(config, platform, name);

        match apply_server_to_platform(platform, &config_path, name, &adapted) {
            Ok(_) => {
                println!("    \x1b[32m✓\x1b[0m {}", platform.name);
                ok += 1;
            }
            Err(error) => {
                println!("    \x1b[31m✗\x1b[0m {}: {error}", platform.name);
                fail += 1;
            }
        }
    }

    (ok, fail)
}

// ── Remove ─────────────────────────────────────────────────────────────────

/// Show every unique MCP server found in any platform and remove the chosen
/// one from every platform where it exists.
fn run_remove(home: &Path, detected: &[&Platform]) -> Result<()> {
    println!();
    println!("  \x1b[1mRemove MCP Server\x1b[0m");
    println!("  ─────────────────────────────────────────────");

    let all_configs = collect_all_platform_configs(home, detected);
    let Some(selected) = prompt_server_to_remove(&all_configs)? else {
        return Ok(());
    };

    let target_platforms = target_platforms_for_server(detected, &all_configs, &selected);
    if !confirm(&format!(
        "Will remove \x1b[1m{selected}\x1b[0m from: {}. Proceed?",
        target_platforms
            .iter()
            .map(|platform| platform.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))? {
        println!("  Cancelled.");
        return Ok(());
    }

    let (ok, fail) = remove_server_from_platforms(home, &target_platforms, &selected);

    println!();
    if fail == 0 {
        println!("  \x1b[32m✓\x1b[0m '{selected}' removed from {ok} platform(s).");
    } else {
        println!("  \x1b[33m⚠\x1b[0m Removed from {ok}, failed on {fail}.");
    }

    println!();
    show_updated_table(home, detected);
    Ok(())
}

fn prompt_server_to_remove(all_configs: &PlatformConfigs) -> Result<Option<String>> {
    let all_names = collect_all_server_names(all_configs);
    if all_names.is_empty() {
        println!("  \x1b[33m⚠\x1b[0m  No MCP servers found.");
        return Ok(None);
    }

    let choices: Vec<String> = all_names.into_iter().collect();
    let selected = Select::new("Select server to remove:", choices)
        .with_help_message("Enter to confirm | Esc to cancel")
        .prompt()
        .map_err(cancelled_prompt)?;

    Ok(Some(selected))
}

fn collect_all_server_names(all_configs: &PlatformConfigs) -> BTreeSet<String> {
    all_configs
        .values()
        .flat_map(|servers| servers.keys().cloned())
        .collect()
}

fn target_platforms_for_server<'a>(
    detected: &'a [&Platform],
    all_configs: &PlatformConfigs,
    server_name: &str,
) -> Vec<&'a Platform> {
    detected
        .iter()
        .copied()
        .filter(|platform| {
            all_configs
                .get(&platform.name)
                .is_some_and(|servers| servers.contains_key(server_name))
        })
        .collect()
}

fn remove_server_from_platforms(
    home: &Path,
    target_platforms: &[&Platform],
    server_name: &str,
) -> (usize, usize) {
    let mut ok = 0usize;
    let mut fail = 0usize;

    for platform in target_platforms {
        let config_path = resolve_platform_config_path(home, platform);
        match remove_server_from_platform(platform, &config_path, server_name) {
            Ok(true) => {
                println!("    \x1b[32m✓\x1b[0m {}", platform.name);
                ok += 1;
            }
            Ok(false) => println!(
                "    \x1b[33m–\x1b[0m {} (not found, skipped)",
                platform.name
            ),
            Err(error) => {
                println!("    \x1b[31m✗\x1b[0m {}: {error}", platform.name);
                fail += 1;
            }
        }
    }

    (ok, fail)
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn clear_screen() -> Result<()> {
    print!("\x1b[2J\x1b[H");
    io::stdout().flush()?;
    Ok(())
}

fn fetch_registry() -> Result<crate::setup_module::models::RegistryRaw> {
    print!("  Fetching platform registry… ");
    io::stdout().flush()?;
    let registry = setup::fetch_registry_raw().context("Failed to fetch registry")?;
    println!("\x1b[32m✓\x1b[0m");
    Ok(registry)
}

fn detect_available_platforms(platforms: &[Platform]) -> Vec<&Platform> {
    platforms
        .iter()
        .filter(|platform| setup::is_platform_available(platform))
        .collect()
}

fn print_no_supported_platforms(platforms: &[Platform]) {
    println!(
        "  \x1b[33m⚠\x1b[0m  No supported platforms detected ({}). Install one and re-run.",
        platforms
            .iter()
            .map(|platform| platform.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn print_detected_platforms(detected: &[&Platform]) {
    println!(
        "  Platforms detected: \x1b[32m{}\x1b[0m",
        detected
            .iter()
            .map(|platform| platform.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
}

fn prompt_wizard_action() -> Result<Option<WizardAction>> {
    let action = Select::new(
        "What would you like to do?",
        vec![
            "Sync — replicate MCPs across all platforms",
            "Add — register a new MCP server everywhere",
            "Remove — delete an MCP server from all platforms",
        ],
    )
    .with_help_message("↑↓ navigate | Enter select | Esc cancel")
    .prompt()
    .map_err(cancelled_prompt)?;

    Ok(match action {
        a if a.starts_with("Sync") => Some(WizardAction::Sync),
        a if a.starts_with("Add") => Some(WizardAction::Add),
        a if a.starts_with("Remove") => Some(WizardAction::Remove),
        _ => None,
    })
}

fn cancelled_prompt(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("Cancelled: {}", error)
}

fn prompt_trimmed_text(prompt: &str) -> Result<String> {
    Text::new(prompt)
        .prompt()
        .map(|value| value.trim().to_string())
        .map_err(cancelled_prompt)
}

fn prompt_server_type() -> Result<String> {
    Select::new("Server type:", vec!["http", "remote", "stdio"])
        .prompt()
        .map(str::to_owned)
        .map_err(cancelled_prompt)
}

fn validate_required_input(value: &str, error_message: &str) -> bool {
    if !value.is_empty() {
        return true;
    }

    println!("  \x1b[31m✗\x1b[0m {error_message}");
    false
}

/// Ask the user to confirm with [Y/n]. Returns `false` if the user typed "n".
fn confirm(prompt: &str) -> Result<bool> {
    print!("  {prompt} [Y/n] ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_lowercase() != "n")
}

/// Show the updated MCP matrix after an operation.
fn show_updated_table(home: &Path, detected: &[&Platform]) {
    let post_configs = collect_all_platform_configs(home, detected);
    print_mcp_table(detected, &post_configs);
}

fn collect_all_platform_configs(home: &Path, detected: &[&Platform]) -> PlatformConfigs {
    detected
        .iter()
        .map(|platform| (platform.name.clone(), read_platform_config(home, platform)))
        .collect()
}

fn read_platform_config(home: &Path, platform: &Platform) -> BTreeMap<String, serde_json::Value> {
    let config_path = resolve_platform_config_path(home, platform);
    if !config_path.exists() {
        return BTreeMap::new();
    }

    match crate::config::McpConfigRegistry::extract_from_platform(
        &platform.name,
        &config_path,
        &platform.mcp_servers_key,
    ) {
        Ok(config) => config
            .servers
            .into_iter()
            .map(|server| (server.name, server.config))
            .collect(),
        Err(error) => {
            eprintln!(
                "  \x1b[33m⚠\x1b[0m Warning: could not read {} config: {}",
                platform.name, error
            );
            BTreeMap::new()
        }
    }
}

fn existing_server_names(all_configs: &PlatformConfigs, platform_name: &str) -> BTreeSet<String> {
    all_configs
        .get(platform_name)
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default()
}

/// Resolve the config file path for a platform (mirrors `setup::resolve_config_path`).
fn resolve_platform_config_path(home: &Path, platform: &Platform) -> PathBuf {
    let primary = home.join(&platform.config_path);
    if primary.exists() {
        return primary;
    }

    let ext = primary
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    let alternate = match ext {
        "jsonc" => primary.with_extension("json"),
        "json" => primary.with_extension("jsonc"),
        _ => return primary,
    };

    if alternate.exists() {
        return alternate;
    }

    primary
}

/// Write a server config entry to a platform's config file.
fn apply_server_to_platform(
    platform: &Platform,
    config_path: &Path,
    server_name: &str,
    config: &serde_json::Value,
) -> Result<bool> {
    ensure_platform_config_exists(platform, config_path)?;

    if is_toml_platform(platform) {
        return upsert_toml_server(platform, config_path, server_name, config);
    }

    upsert_json_server(platform, config_path, server_name, config)
}

fn ensure_platform_config_exists(platform: &Platform, config_path: &Path) -> Result<()> {
    if config_path.exists() {
        return Ok(());
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(config_path, initial_platform_config(platform))?;
    Ok(())
}

fn initial_platform_config(platform: &Platform) -> String {
    if is_toml_platform(platform) {
        return String::new();
    }

    format!("{{\"{}\": {{}}}}\n", primary_servers_key(platform))
}

fn is_toml_platform(platform: &Platform) -> bool {
    platform.config_format.as_deref() == Some("toml")
}

fn primary_servers_key(platform: &Platform) -> &str {
    platform
        .mcp_servers_key
        .first()
        .map(|key| key.as_str())
        .unwrap_or("mcpServers")
}

fn upsert_toml_server(
    platform: &Platform,
    config_path: &Path,
    server_name: &str,
    config: &serde_json::Value,
) -> Result<bool> {
    if platform.toml_array_format {
        return setup::upsert_toml_array_pub(
            config_path,
            &platform.mcp_servers_key.join("."),
            server_name,
            config,
        );
    }

    setup::upsert_toml_key_pub(
        config_path,
        primary_servers_key(platform),
        server_name,
        config,
    )
}

fn upsert_json_server(
    platform: &Platform,
    config_path: &Path,
    server_name: &str,
    config: &serde_json::Value,
) -> Result<bool> {
    let mut key_refs: Vec<&str> = platform
        .mcp_servers_key
        .iter()
        .map(|key| key.as_str())
        .collect();
    key_refs.push(server_name);
    setup::upsert_json_key_pub(config_path, &key_refs, config)
}

/// Remove a server entry from a platform config file.
fn remove_server_from_platform(
    platform: &Platform,
    config_path: &Path,
    server_name: &str,
) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }

    if is_toml_platform(platform) {
        return setup::remove_toml_server_pub(platform, config_path, server_name);
    }

    setup::remove_json_key_pub(config_path, primary_servers_key(platform), server_name)
}

// ── Banner ─────────────────────────────────────────────────────────────────

use crate::shared::banner;

fn print_mcp_banner() {
    banner::print_banner_with_gradient("Agent Hub — MCP Manager");
    // Removed duplicate line - banner function already prints the separator line
}

// ── Matrix table ───────────────────────────────────────────────────────────

/// Print a table: platforms as columns, MCP servers as rows, ✓/✗ cells.
fn print_mcp_table(detected: &[&Platform], all_configs: &PlatformConfigs) {
    let all_servers = collect_all_server_names(all_configs);
    if all_servers.is_empty() {
        println!("  \x1b[90mNo MCP servers configured.\x1b[0m");
        println!();
        return;
    }

    let name_col = all_servers
        .iter()
        .map(|server| server.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let plat_col = detected
        .iter()
        .map(|platform| platform.name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    print_mcp_table_header(detected, name_col, plat_col);

    for server in &all_servers {
        print_mcp_table_row(server, detected, all_configs, name_col, plat_col);
    }
    println!();
}

fn print_mcp_table_header(detected: &[&Platform], name_col: usize, plat_col: usize) {
    print!("  {:<name_col$}", "Server");
    for platform in detected {
        print!("  {:>plat_col$}", platform.name);
    }
    println!();

    let total_w = name_col + detected.len() * (plat_col + 2);
    println!("  {:─<total_w$}", "");
}

fn print_mcp_table_row(
    server: &str,
    detected: &[&Platform],
    all_configs: &PlatformConfigs,
    name_col: usize,
    plat_col: usize,
) {
    print!("  {:<name_col$}", server);
    for platform in detected {
        let has_server = all_configs
            .get(&platform.name)
            .is_some_and(|servers| servers.contains_key(server));
        let pad = plat_col.saturating_sub(1);
        print!("  {}{}", " ".repeat(pad), server_presence_icon(has_server));
    }
    println!();
}

fn server_presence_icon(has_server: bool) -> &'static str {
    if has_server {
        "\x1b[32m ✓\x1b[0m"
    } else {
        "\x1b[31m ✗\x1b[0m"
    }
}
