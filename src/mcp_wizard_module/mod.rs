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
    server_type: String,
    transport: ServerTransport,
}

enum ServerTransport {
    Url(String),
    Command { command: String, args: Vec<String> },
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

    let config = build_server_config(&input);

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

    let server_type = prompt_server_type()?;
    let Some(transport) = prompt_server_transport(&server_type)? else {
        return Ok(None);
    };

    Ok(Some(AddServerInput {
        name,
        server_type,
        transport,
    }))
}

fn prompt_server_transport(server_type: &str) -> Result<Option<ServerTransport>> {
    if server_type != "stdio" {
        let url = prompt_trimmed_text("Server URL (e.g. \"https://example.com/mcp\"):")?;
        if !validate_required_input(&url, "Server URL is required.") {
            return Ok(None);
        }
        return Ok(Some(ServerTransport::Url(url)));
    }

    let command_line = prompt_trimmed_text("Command (e.g. \"npx -y @scope/server\"):")?;
    if !validate_required_input(&command_line, "Command is required.") {
        return Ok(None);
    }

    let mut parts = command_line.split_whitespace().map(str::to_owned);
    let command = parts.next().unwrap_or_default();
    Ok(Some(ServerTransport::Command {
        command,
        args: parts.collect(),
    }))
}

fn build_server_config(input: &AddServerInput) -> serde_json::Value {
    match &input.transport {
        ServerTransport::Url(url) => serde_json::json!({
            "type": input.server_type,
            "url": url,
        }),
        ServerTransport::Command { command, args } => serde_json::json!({
            "type": input.server_type,
            "command": command,
            "args": args,
        }),
    }
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

// ── Matrix table / cards ────────────────────────────────────────────────────

/// Fallback terminal width used when the real size can't be detected (e.g.
/// non-interactive test runs), matching the convention used across the TUI.
const DEFAULT_TERM_WIDTH: usize = 120;

/// Print platforms-vs-MCPs as a matrix table (columns = platforms) when it
/// fits the terminal width, or as one card per platform otherwise. The matrix
/// layout grows a column per platform, so past a handful of platforms it
/// wraps and misaligns; the card view scales to any number of platforms.
fn print_mcp_table(detected: &[&Platform], all_configs: &PlatformConfigs) {
    print!("{}", render_mcp_table(detected, all_configs));
}

fn render_mcp_table(detected: &[&Platform], all_configs: &PlatformConfigs) -> String {
    let all_servers = collect_all_server_names(all_configs);
    if all_servers.is_empty() {
        return "  \x1b[90mNo MCP servers configured.\x1b[0m\n\n".to_string();
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

    let matrix_width = 2 + name_col + detected.len() * (plat_col + 2);
    let term_width = ratatui::crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(DEFAULT_TERM_WIDTH);

    if matrix_width > term_width.max(DEFAULT_TERM_WIDTH) {
        render_mcp_cards(&all_servers, detected, all_configs, name_col)
    } else {
        render_mcp_matrix(&all_servers, detected, all_configs, name_col, plat_col)
    }
}

fn render_mcp_matrix(
    all_servers: &BTreeSet<String>,
    detected: &[&Platform],
    all_configs: &PlatformConfigs,
    name_col: usize,
    plat_col: usize,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    write!(out, "  {:<name_col$}", "Server").unwrap();
    for platform in detected {
        write!(out, "  {:>plat_col$}", platform.name).unwrap();
    }
    out.push('\n');

    let total_w = name_col + detected.len() * (plat_col + 2);
    writeln!(out, "  {:─<total_w$}", "").unwrap();

    for server in all_servers {
        write!(out, "  {server:<name_col$}").unwrap();
        for platform in detected {
            let has_server = all_configs
                .get(&platform.name)
                .is_some_and(|servers| servers.contains_key(server));
            let pad = plat_col.saturating_sub(1);
            write!(
                out,
                "  {}{}",
                " ".repeat(pad),
                server_presence_icon(has_server)
            )
            .unwrap();
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

/// One section per platform listing every known MCP server with its ✓/✗,
/// used instead of the matrix when there are too many platforms to fit as
/// columns.
fn render_mcp_cards(
    all_servers: &BTreeSet<String>,
    detected: &[&Platform],
    all_configs: &PlatformConfigs,
    name_col: usize,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for platform in detected {
        writeln!(out, "  \x1b[1m{}\x1b[0m", platform.name).unwrap();
        let empty = BTreeMap::new();
        let servers = all_configs.get(&platform.name).unwrap_or(&empty);
        for server in all_servers {
            let has_server = servers.contains_key(server);
            writeln!(
                out,
                "    {server:<name_col$}  {}",
                server_presence_icon(has_server)
            )
            .unwrap();
        }
        out.push('\n');
    }
    out
}

fn server_presence_icon(has_server: bool) -> &'static str {
    if has_server {
        "\x1b[32m ✓\x1b[0m"
    } else {
        "\x1b[31m ✗\x1b[0m"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup_module::Platform;

    fn test_platform(name: &str) -> Platform {
        Platform {
            name: name.to_string(),
            config_path: format!("{name}.json"),
            config_format: Some("json".to_string()),
            toml_array_format: false,
            command_format: "separate".to_string(),
            mcp_servers_key: vec!["mcpServers".to_string()],
            deprecated_keys: Vec::new(),
            unsupported_keys: Vec::new(),
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: None,
        }
    }

    /// 8 platforms with reasonably long names and 10 MCP servers: the matrix
    /// layout would need well over 120 columns, so this must render as cards.
    #[test]
    fn render_mcp_table_switches_to_cards_for_many_platforms() {
        let platforms: Vec<Platform> = (1..=8)
            .map(|i| test_platform(&format!("platform-name-{i:02}")))
            .collect();
        let detected: Vec<&Platform> = platforms.iter().collect();

        let server_names: Vec<String> = (1..=10).map(|i| format!("mcp-server-{i:02}")).collect();

        let mut all_configs: PlatformConfigs = BTreeMap::new();
        for (p_idx, platform) in platforms.iter().enumerate() {
            let mut servers = BTreeMap::new();
            for (s_idx, server) in server_names.iter().enumerate() {
                // Deterministic, mixed presence pattern.
                if (p_idx + s_idx) % 2 == 0 {
                    servers.insert(server.clone(), serde_json::json!({}));
                }
            }
            all_configs.insert(platform.name.clone(), servers);
        }

        let rendered = render_mcp_table(&detected, &all_configs);

        for line in rendered.lines() {
            let visible_len = strip_ansi(line).chars().count();
            assert!(
                visible_len <= 120,
                "line exceeds 120 visible chars ({visible_len}): {line:?}"
            );
        }

        for (p_idx, platform) in platforms.iter().enumerate() {
            assert!(
                rendered.contains(&platform.name),
                "missing platform section for {}",
                platform.name
            );
            for (s_idx, server) in server_names.iter().enumerate() {
                let expects_present = (p_idx + s_idx) % 2 == 0;
                let icon = if expects_present { '✓' } else { '✗' };
                let needle = server.to_string();
                let section_start = rendered.find(&platform.name).unwrap();
                let next_section = platforms
                    .get(p_idx + 1)
                    .and_then(|next| rendered.find(&next.name))
                    .unwrap_or(rendered.len());
                let section = &rendered[section_start..next_section];
                let line = section
                    .lines()
                    .find(|l| l.contains(&needle))
                    .unwrap_or_else(|| panic!("missing {server} in section for {}", platform.name));
                assert!(
                    line.contains(icon),
                    "expected {icon} for {server} in {}, got: {line:?}",
                    platform.name
                );
            }
        }
    }

    #[test]
    fn render_mcp_table_uses_matrix_for_few_platforms() {
        let platforms = [test_platform("cursor"), test_platform("claude")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut all_configs: PlatformConfigs = BTreeMap::new();
        let mut cursor_servers = BTreeMap::new();
        cursor_servers.insert("fs".to_string(), serde_json::json!({}));
        all_configs.insert("cursor".to_string(), cursor_servers);
        all_configs.insert("claude".to_string(), BTreeMap::new());

        let rendered = render_mcp_table(&detected, &all_configs);
        assert!(rendered.contains("Server"));
        assert!(rendered.contains('✓'));
        assert!(rendered.contains('✗'));
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for esc in chars.by_ref() {
                    if esc == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    // ── build_unified_servers ─────────────────────────────────────────────

    #[test]
    fn build_unified_servers_empty_when_no_configs() {
        let configs: PlatformConfigs = BTreeMap::new();
        let unified = build_unified_servers(&configs);
        assert!(unified.is_empty());
    }

    #[test]
    fn build_unified_servers_collects_from_single_platform() {
        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut servers = BTreeMap::new();
        servers.insert("github".to_string(), serde_json::json!({"url": "a"}));
        servers.insert("slack".to_string(), serde_json::json!({"url": "b"}));
        configs.insert("cursor".to_string(), servers);

        let unified = build_unified_servers(&configs);
        assert_eq!(unified.len(), 2);
        assert!(unified.contains_key("github"));
        assert!(unified.contains_key("slack"));
    }

    #[test]
    fn build_unified_servers_first_platform_wins_on_duplicate_names() {
        let mut configs: PlatformConfigs = BTreeMap::new();

        let mut cursor = BTreeMap::new();
        cursor.insert("github".to_string(), serde_json::json!({"url": "from-cursor"}));
        configs.insert("cursor".to_string(), cursor);

        let mut claude = BTreeMap::new();
        claude.insert("github".to_string(), serde_json::json!({"url": "from-claude"}));
        configs.insert("claude".to_string(), claude);

        let unified = build_unified_servers(&configs);
        let (val, platform) = unified.get("github").unwrap();
        // BTreeMap iterates in sorted key order, so "claude" comes before "cursor"
        assert_eq!(platform, &"claude");
        assert_eq!(val, &serde_json::json!({"url": "from-claude"}));
    }

    #[test]
    fn build_unified_servers_preserves_source_platform_ref() {
        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut servers = BTreeMap::new();
        servers.insert("x".to_string(), serde_json::json!({}));
        configs.insert("myplatform".to_string(), servers);

        let unified = build_unified_servers(&configs);
        assert_eq!(unified.get("x").unwrap().1, "myplatform");
    }

    // ── collect_missing_servers ───────────────────────────────────────────

    #[test]
    fn collect_missing_servers_empty_when_all_in_sync() {
        let platforms = [test_platform("cursor")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut all_configs: PlatformConfigs = BTreeMap::new();
        let mut servers = BTreeMap::new();
        servers.insert("github".to_string(), serde_json::json!({}));
        all_configs.insert("cursor".to_string(), servers);

        let mut unified: UnifiedServers = BTreeMap::new();
        unified.insert("github".to_string(), (serde_json::json!({}), "cursor"));

        let missing = collect_missing_servers(&detected, &all_configs, &unified);
        assert!(missing.is_empty());
    }

    #[test]
    fn collect_missing_servers_finds_gaps() {
        let platforms = [test_platform("cursor"), test_platform("claude")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut all_configs: PlatformConfigs = BTreeMap::new();
        let mut cursor = BTreeMap::new();
        cursor.insert("github".to_string(), serde_json::json!({}));
        all_configs.insert("cursor".to_string(), cursor);
        all_configs.insert("claude".to_string(), BTreeMap::new());

        let mut unified: UnifiedServers = BTreeMap::new();
        unified.insert("github".to_string(), (serde_json::json!({}), "cursor"));

        let missing = collect_missing_servers(&detected, &all_configs, &unified);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "claude");
        assert_eq!(missing[0].1, "github");
    }

    #[test]
    fn collect_missing_servers_empty_when_no_platforms() {
        let detected: Vec<&Platform> = vec![];
        let all_configs: PlatformConfigs = BTreeMap::new();
        let unified: UnifiedServers = BTreeMap::new();

        let missing = collect_missing_servers(&detected, &all_configs, &unified);
        assert!(missing.is_empty());
    }

    #[test]
    fn collect_missing_servers_handles_platform_absent_from_configs() {
        let platforms = [test_platform("cursor"), test_platform("claude")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        // "claude" has no entry at all in all_configs
        let mut all_configs: PlatformConfigs = BTreeMap::new();
        let mut cursor = BTreeMap::new();
        cursor.insert("github".to_string(), serde_json::json!({}));
        all_configs.insert("cursor".to_string(), cursor);

        let mut unified: UnifiedServers = BTreeMap::new();
        unified.insert("github".to_string(), (serde_json::json!({}), "cursor"));

        let missing = collect_missing_servers(&detected, &all_configs, &unified);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "claude");
    }

    #[test]
    fn collect_missing_servers_multiple_servers_missing_on_multiple_platforms() {
        let platforms = [test_platform("a"), test_platform("b")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let all_configs: PlatformConfigs = BTreeMap::new();
        // Neither platform has any servers
        let mut unified: UnifiedServers = BTreeMap::new();
        unified.insert("s1".to_string(), (serde_json::json!({}), "a"));
        unified.insert("s2".to_string(), (serde_json::json!({}), "b"));

        let missing = collect_missing_servers(&detected, &all_configs, &unified);
        // 2 platforms x 2 servers = 4 missing entries
        assert_eq!(missing.len(), 4);
    }

    // ── existing_server_names ─────────────────────────────────────────────

    #[test]
    fn existing_server_names_returns_empty_for_unknown_platform() {
        let configs: PlatformConfigs = BTreeMap::new();
        let names = existing_server_names(&configs, "nonexistent");
        assert!(names.is_empty());
    }

    #[test]
    fn existing_server_names_returns_all_names() {
        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut servers = BTreeMap::new();
        servers.insert("a".to_string(), serde_json::json!({}));
        servers.insert("b".to_string(), serde_json::json!({}));
        servers.insert("c".to_string(), serde_json::json!({}));
        configs.insert("cursor".to_string(), servers);

        let names = existing_server_names(&configs, "cursor");
        assert_eq!(names.len(), 3);
        assert!(names.contains("a"));
        assert!(names.contains("b"));
        assert!(names.contains("c"));
    }

    // ── collect_all_server_names ──────────────────────────────────────────

    #[test]
    fn collect_all_server_names_empty() {
        let configs: PlatformConfigs = BTreeMap::new();
        let names = collect_all_server_names(&configs);
        assert!(names.is_empty());
    }

    #[test]
    fn collect_all_server_names_deduplicates_across_platforms() {
        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut cursor = BTreeMap::new();
        cursor.insert("github".to_string(), serde_json::json!({}));
        cursor.insert("slack".to_string(), serde_json::json!({}));
        configs.insert("cursor".to_string(), cursor);

        let mut claude = BTreeMap::new();
        claude.insert("github".to_string(), serde_json::json!({}));
        claude.insert("notion".to_string(), serde_json::json!({}));
        configs.insert("claude".to_string(), claude);

        let names = collect_all_server_names(&configs);
        assert_eq!(names.len(), 3);
        assert!(names.contains("github"));
        assert!(names.contains("slack"));
        assert!(names.contains("notion"));
    }

    #[test]
    fn collect_all_server_names_deterministic_order() {
        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut s1 = BTreeMap::new();
        s1.insert("z-server".to_string(), serde_json::json!({}));
        s1.insert("a-server".to_string(), serde_json::json!({}));
        configs.insert("p1".to_string(), s1);

        let names: Vec<String> = collect_all_server_names(&configs).into_iter().collect();
        assert_eq!(names, vec!["a-server", "z-server"]);
    }

    // ── target_platforms_for_server ───────────────────────────────────────

    #[test]
    fn target_platforms_for_server_none_when_absent() {
        let platforms = [test_platform("a"), test_platform("b")];
        let detected: Vec<&Platform> = platforms.iter().collect();
        let all_configs: PlatformConfigs = BTreeMap::new();

        let targets = target_platforms_for_server(&detected, &all_configs, "x");
        assert!(targets.is_empty());
    }

    #[test]
    fn target_platforms_for_server_filters_correctly() {
        let platforms = [test_platform("a"), test_platform("b"), test_platform("c")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut all_configs: PlatformConfigs = BTreeMap::new();
        let mut a_servers = BTreeMap::new();
        a_servers.insert("github".to_string(), serde_json::json!({}));
        all_configs.insert("a".to_string(), a_servers);
        // "b" has no servers
        let mut c_servers = BTreeMap::new();
        c_servers.insert("github".to_string(), serde_json::json!({}));
        all_configs.insert("c".to_string(), c_servers);

        let targets = target_platforms_for_server(&detected, &all_configs, "github");
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().any(|p| p.name == "a"));
        assert!(targets.iter().any(|p| p.name == "c"));
        assert!(!targets.iter().any(|p| p.name == "b"));
    }

    // ── resolve_platform_config_path ──────────────────────────────────────

    #[test]
    fn resolve_platform_config_path_returns_primary_when_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_file = home.join("test.json");
        std::fs::write(&config_file, "{}").unwrap();

        let platform = Platform {
            config_path: "test.json".to_string(),
            ..test_platform("test")
        };

        let resolved = resolve_platform_config_path(home, &platform);
        assert_eq!(resolved, config_file);
    }

    #[test]
    fn resolve_platform_config_path_falls_back_jsonc_to_json() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // Primary is .jsonc, but .json exists
        std::fs::write(home.join("cfg.json"), "{}").unwrap();

        let platform = Platform {
            config_path: "cfg.jsonc".to_string(),
            ..test_platform("test")
        };

        let resolved = resolve_platform_config_path(home, &platform);
        assert_eq!(resolved.extension().unwrap(), "json");
    }

    #[test]
    fn resolve_platform_config_path_falls_back_json_to_jsonc() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(home.join("cfg.jsonc"), "{}").unwrap();

        let platform = Platform {
            config_path: "cfg.json".to_string(),
            ..test_platform("test")
        };

        let resolved = resolve_platform_config_path(home, &platform);
        assert_eq!(resolved.extension().unwrap(), "jsonc");
    }

    #[test]
    fn resolve_platform_config_path_returns_primary_when_neither_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let platform = Platform {
            config_path: "nonexistent.json".to_string(),
            ..test_platform("test")
        };

        let resolved = resolve_platform_config_path(home, &platform);
        assert_eq!(resolved, home.join("nonexistent.json"));
    }

    #[test]
    fn resolve_platform_config_path_primary_takes_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(home.join("cfg.jsonc"), "{}").unwrap();
        std::fs::write(home.join("cfg.json"), "{}").unwrap();

        let platform = Platform {
            config_path: "cfg.jsonc".to_string(),
            ..test_platform("test")
        };

        let resolved = resolve_platform_config_path(home, &platform);
        assert_eq!(resolved.extension().unwrap(), "jsonc");
    }

    #[test]
    fn resolve_platform_config_path_unknown_extension_no_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // .yaml doesn't have a known alternate
        let platform = Platform {
            config_path: "cfg.yaml".to_string(),
            ..test_platform("test")
        };

        let resolved = resolve_platform_config_path(home, &platform);
        assert_eq!(resolved, home.join("cfg.yaml"));
    }

    // ── validate_required_input ───────────────────────────────────────────

    #[test]
    fn validate_required_input_passes_non_empty() {
        assert!(validate_required_input("hello", "err"));
    }

    #[test]
    fn validate_required_input_rejects_empty() {
        assert!(!validate_required_input("", "err"));
    }

    #[test]
    fn validate_required_input_accepts_whitespace_only() {
        // trim happens at the caller level, so raw whitespace is "valid" here
        assert!(validate_required_input("  ", "err"));
    }

    #[test]
    fn validate_required_input_accepts_single_char() {
        assert!(validate_required_input("a", "err"));
    }

    // ── build_server_config ───────────────────────────────────────────────

    #[test]
    fn build_server_config_url_type() {
        let input = AddServerInput {
            name: "test".to_string(),
            server_type: "http".to_string(),
            transport: ServerTransport::Url("https://example.com/mcp".to_string()),
        };

        let config = build_server_config(&input);
        assert_eq!(config["type"], "http");
        assert_eq!(config["url"], "https://example.com/mcp");
        assert!(config.get("command").is_none());
    }

    #[test]
    fn build_server_config_stdio_type_with_args() {
        let input = AddServerInput {
            name: "test".to_string(),
            server_type: "stdio".to_string(),
            transport: ServerTransport::Command {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@scope/server".to_string()],
            },
        };

        let config = build_server_config(&input);
        assert_eq!(config["type"], "stdio");
        assert_eq!(config["command"], "npx");
        assert_eq!(config["args"], serde_json::json!(["-y", "@scope/server"]));
        assert!(config.get("url").is_none());
    }

    #[test]
    fn build_server_config_stdio_no_args() {
        let input = AddServerInput {
            name: "test".to_string(),
            server_type: "stdio".to_string(),
            transport: ServerTransport::Command {
                command: "node".to_string(),
                args: vec![],
            },
        };

        let config = build_server_config(&input);
        assert_eq!(config["command"], "node");
        assert_eq!(config["args"], serde_json::json!([]));
    }

    // ── server_presence_icon ──────────────────────────────────────────────

    #[test]
    fn server_presence_icon_checkmark() {
        let icon = server_presence_icon(true);
        assert!(icon.contains('✓'));
    }

    #[test]
    fn server_presence_icon_cross() {
        let icon = server_presence_icon(false);
        assert!(icon.contains('✗'));
    }

    #[test]
    fn server_presence_icon_has_color_codes() {
        let icon = server_presence_icon(true);
        assert!(icon.contains("\x1b[32m")); // green
        let icon = server_presence_icon(false);
        assert!(icon.contains("\x1b[31m")); // red
    }

    // ── is_toml_platform ─────────────────────────────────────────────────

    #[test]
    fn is_toml_platform_true_when_format_is_toml() {
        let p = Platform {
            config_format: Some("toml".to_string()),
            ..test_platform("x")
        };
        assert!(is_toml_platform(&p));
    }

    #[test]
    fn is_toml_platform_false_for_json() {
        let p = Platform {
            config_format: Some("json".to_string()),
            ..test_platform("x")
        };
        assert!(!is_toml_platform(&p));
    }

    #[test]
    fn is_toml_platform_false_for_none() {
        let p = Platform {
            config_format: None,
            ..test_platform("x")
        };
        assert!(!is_toml_platform(&p));
    }

    // ── primary_servers_key ───────────────────────────────────────────────

    #[test]
    fn primary_servers_key_uses_first_key() {
        let p = Platform {
            mcp_servers_key: vec!["servers".to_string(), "mcpServers".to_string()],
            ..test_platform("x")
        };
        assert_eq!(primary_servers_key(&p), "servers");
    }

    #[test]
    fn primary_servers_key_defaults_to_mcp_servers_when_empty() {
        let p = Platform {
            mcp_servers_key: vec![],
            ..test_platform("x")
        };
        assert_eq!(primary_servers_key(&p), "mcpServers");
    }

    // ── initial_platform_config ───────────────────────────────────────────

    #[test]
    fn initial_platform_config_toml_returns_empty_string() {
        let p = Platform {
            config_format: Some("toml".to_string()),
            ..test_platform("x")
        };
        assert_eq!(initial_platform_config(&p), "");
    }

    #[test]
    fn initial_platform_config_json_uses_primary_key() {
        let p = Platform {
            config_format: Some("json".to_string()),
            mcp_servers_key: vec!["mcpServers".to_string()],
            ..test_platform("x")
        };
        let cfg = initial_platform_config(&p);
        assert!(cfg.contains("\"mcpServers\""));
        assert!(cfg.ends_with('\n'));
    }

    #[test]
    fn initial_platform_config_json_custom_key() {
        let p = Platform {
            config_format: Some("json".to_string()),
            mcp_servers_key: vec!["servers".to_string()],
            ..test_platform("x")
        };
        let cfg = initial_platform_config(&p);
        assert!(cfg.contains("\"servers\""));
    }

    // ── render_mcp_table ──────────────────────────────────────────────────

    #[test]
    fn render_mcp_table_empty_message() {
        let detected: Vec<&Platform> = vec![];
        let configs: PlatformConfigs = BTreeMap::new();
        let rendered = render_mcp_table(&detected, &configs);
        assert!(rendered.contains("No MCP servers configured."));
    }

    #[test]
    fn render_mcp_table_contains_server_names() {
        let platforms = [test_platform("cursor")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut servers = BTreeMap::new();
        servers.insert("github".to_string(), serde_json::json!({}));
        servers.insert("slack".to_string(), serde_json::json!({}));
        configs.insert("cursor".to_string(), servers);

        let rendered = render_mcp_table(&detected, &configs);
        assert!(rendered.contains("github"));
        assert!(rendered.contains("slack"));
    }

    #[test]
    fn render_mcp_table_shows_checkmarks_for_present() {
        let platforms = [test_platform("cursor")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut servers = BTreeMap::new();
        servers.insert("github".to_string(), serde_json::json!({}));
        configs.insert("cursor".to_string(), servers);

        let rendered = render_mcp_table(&detected, &configs);
        assert!(rendered.contains('✓'));
    }

    // ── render_mcp_cards ──────────────────────────────────────────────────

    #[test]
    fn render_mcp_cards_shows_platform_headers() {
        let platforms = [test_platform("p1"), test_platform("p2")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut all_servers = BTreeSet::new();
        all_servers.insert("github".to_string());

        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut s1 = BTreeMap::new();
        s1.insert("github".to_string(), serde_json::json!({}));
        configs.insert("p1".to_string(), s1);
        configs.insert("p2".to_string(), BTreeMap::new());

        let rendered = render_mcp_cards(&all_servers, &detected, &configs, 6);
        assert!(rendered.contains("p1"));
        assert!(rendered.contains("p2"));
        // p1 has github -> checkmark, p2 doesn't -> cross
        let p1_section_start = rendered.find("p1").unwrap();
        let p2_section_start = rendered.find("p2").unwrap();
        let p1_section = &rendered[p1_section_start..p2_section_start];
        assert!(p1_section.contains('✓'));
        let p2_section = &rendered[p2_section_start..];
        assert!(p2_section.contains('✗'));
    }

    // ── render_mcp_matrix ─────────────────────────────────────────────────

    #[test]
    fn render_mcp_matrix_header_row() {
        let platforms = [test_platform("cursor"), test_platform("claude")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut all_servers = BTreeSet::new();
        all_servers.insert("github".to_string());

        let mut configs: PlatformConfigs = BTreeMap::new();
        let mut cursor = BTreeMap::new();
        cursor.insert("github".to_string(), serde_json::json!({}));
        configs.insert("cursor".to_string(), cursor);
        configs.insert("claude".to_string(), BTreeMap::new());

        let rendered = render_mcp_matrix(&all_servers, &detected, &configs, 8, 6);
        // Header should contain "Server" and both platform names
        let first_line = rendered.lines().next().unwrap();
        assert!(first_line.contains("Server"));
        assert!(strip_ansi(first_line).contains("cursor"));
        assert!(strip_ansi(first_line).contains("claude"));
    }

    #[test]
    fn render_mcp_matrix_separator_line() {
        let platforms = [test_platform("a")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let mut all_servers = BTreeSet::new();
        all_servers.insert("x".to_string());

        let configs: PlatformConfigs = BTreeMap::new();
        let rendered = render_mcp_matrix(&all_servers, &detected, &configs, 4, 4);

        // Second line should be a separator (all ─)
        let sep_line = rendered.lines().nth(1).unwrap();
        assert!(sep_line.contains('─'));
    }

    // ── strip_ansi (test helper) ──────────────────────────────────────────

    #[test]
    fn strip_ansi_removes_escape_codes() {
        let colored = "\x1b[32m ✓\x1b[0m";
        assert_eq!(strip_ansi(colored), " ✓");
    }

    #[test]
    fn strip_ansi_plain_text_unchanged() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn strip_ansi_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strip_ansi_multiple_codes() {
        let s = "\x1b[1m\x1b[32mtext\x1b[0m";
        assert_eq!(strip_ansi(s), "text");
    }

    // ── test_platform helper edge cases ───────────────────────────────────

    #[test]
    fn test_platform_default_values() {
        let p = test_platform("my-platform");
        assert_eq!(p.name, "my-platform");
        assert_eq!(p.config_path, "my-platform.json");
        assert_eq!(p.config_format, Some("json".to_string()));
        assert!(!p.toml_array_format);
        assert_eq!(p.command_format, "separate");
        assert_eq!(p.mcp_servers_key, vec!["mcpServers".to_string()]);
        assert!(p.deprecated_keys.is_empty());
        assert!(p.unsupported_keys.is_empty());
        assert!(p.fields_mapping.is_empty());
        assert!(p.required_fields.is_empty());
        assert!(p.server_extras.is_empty());
        assert!(p.skills_dir.is_none());
        assert!(p.instruction_file.is_none());
        assert!(p.cli.is_none());
    }

    // ── WizardAction enum ─────────────────────────────────────────────────

    #[test]
    fn wizard_action_clone_copy() {
        let a = WizardAction::Sync;
        let b = a;
        // Copy semantics — both should be valid
        match (a, b) {
            (WizardAction::Sync, WizardAction::Sync) => {}
            _ => panic!("Copy failed"),
        }
    }

    // ── ServerTransport enum ──────────────────────────────────────────────

    #[test]
    fn server_transport_url_variants() {
        let t = ServerTransport::Url("https://x.com".to_string());
        match t {
            ServerTransport::Url(u) => assert_eq!(u, "https://x.com"),
            _ => panic!("Expected Url variant"),
        }
    }

    #[test]
    fn server_transport_command_variants() {
        let t = ServerTransport::Command {
            command: "npx".to_string(),
            args: vec!["-y".to_string()],
        };
        match t {
            ServerTransport::Command { command, args } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y"]);
            }
            _ => panic!("Expected Command variant"),
        }
    }

    // ── prompt_server_transport command parsing (logic test) ───────────────

    #[test]
    fn command_line_splitting_logic() {
        // Replicate the splitting logic from prompt_server_transport
        let command_line = "npx -y @scope/server --verbose";
        let mut parts = command_line.split_whitespace().map(str::to_owned);
        let command = parts.next().unwrap_or_default();
        let args: Vec<String> = parts.collect();

        assert_eq!(command, "npx");
        assert_eq!(args, vec!["-y", "@scope/server", "--verbose"]);
    }

    #[test]
    fn command_line_single_word() {
        let command_line = "node";
        let mut parts = command_line.split_whitespace().map(str::to_owned);
        let command = parts.next().unwrap_or_default();
        let args: Vec<String> = parts.collect();

        assert_eq!(command, "node");
        assert!(args.is_empty());
    }

    #[test]
    fn command_line_empty_string() {
        let command_line = "";
        let mut parts = command_line.split_whitespace().map(str::to_owned);
        let command = parts.next().unwrap_or_default();
        let args: Vec<String> = parts.collect();

        assert_eq!(command, "");
        assert!(args.is_empty());
    }

    #[test]
    fn command_line_only_whitespace() {
        let command_line = "   ";
        let mut parts = command_line.split_whitespace().map(str::to_owned);
        let command = parts.next().unwrap_or_default();
        let args: Vec<String> = parts.collect();

        assert_eq!(command, "");
        assert!(args.is_empty());
    }

    #[test]
    fn command_line_extra_whitespace() {
        let command_line = "  npx   -y   @scope/server  ";
        let mut parts = command_line.split_whitespace().map(str::to_owned);
        let command = parts.next().unwrap_or_default();
        let args: Vec<String> = parts.collect();

        assert_eq!(command, "npx");
        assert_eq!(args, vec!["-y", "@scope/server"]);
    }

    // ── cancelled_prompt ──────────────────────────────────────────────────

    #[test]
    fn cancelled_prompt_contains_message() {
        let err = cancelled_prompt("user pressed Esc");
        assert!(err.to_string().contains("user pressed Esc"));
        assert!(err.to_string().contains("Cancelled"));
    }

    // ── AddServerInput struct ─────────────────────────────────────────────

    #[test]
    fn add_server_input_fields() {
        let input = AddServerInput {
            name: "github".to_string(),
            server_type: "http".to_string(),
            transport: ServerTransport::Url("https://x.com".to_string()),
        };
        assert_eq!(input.name, "github");
        assert_eq!(input.server_type, "http");
    }

    // ── PlatformConfigs / UnifiedServers type aliases ─────────────────────

    #[test]
    fn platform_configs_btreemap_ordering() {
        let mut configs: PlatformConfigs = BTreeMap::new();
        configs.insert("z-platform".to_string(), BTreeMap::new());
        configs.insert("a-platform".to_string(), BTreeMap::new());

        let keys: Vec<&String> = configs.keys().collect();
        assert_eq!(keys, vec!["a-platform", "z-platform"]);
    }

    #[test]
    fn unified_servers_btreemap_ordering() {
        let mut unified: UnifiedServers = BTreeMap::new();
        unified.insert("z-server".to_string(), (serde_json::json!({}), "p"));
        unified.insert("a-server".to_string(), (serde_json::json!({}), "p"));

        let keys: Vec<&String> = unified.keys().collect();
        assert_eq!(keys, vec!["a-server", "z-server"]);
    }

    // ── collect_all_platform_configs ──────────────────────────────────────

    #[test]
    fn collect_all_platform_configs_returns_map_per_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let platforms = [test_platform("cursor"), test_platform("claude")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let configs = collect_all_platform_configs(home, &detected);
        assert_eq!(configs.len(), 2);
        assert!(configs.contains_key("cursor"));
        assert!(configs.contains_key("claude"));
    }

    #[test]
    fn collect_all_platform_configs_empty_when_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let platforms = [test_platform("cursor")];
        let detected: Vec<&Platform> = platforms.iter().collect();

        let configs = collect_all_platform_configs(home, &detected);
        // Config file doesn't exist, so it should return empty map
        assert!(configs["cursor"].is_empty());
    }

    // ── print_sync_summary edge cases ─────────────────────────────────────

    #[test]
    fn print_sync_summary_no_output_panic_test() {
        // Just verify the function doesn't panic with zero errors
        print_sync_summary(5, 0);
    }

    #[test]
    fn print_sync_summary_with_errors() {
        // Verify it doesn't panic with errors
        print_sync_summary(3, 2);
    }

    // ── type alias constraints ────────────────────────────────────────────

    #[test]
    fn missing_servers_is_vector_of_tuples() {
        let missing: MissingServers = vec![
            ("platform".to_string(), "server".to_string()),
        ];
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "platform");
        assert_eq!(missing[0].1, "server");
    }

    // ── Edge: multiple mcp_servers_key paths ──────────────────────────────

    #[test]
    fn primary_servers_key_uses_first_of_multiple() {
        let p = Platform {
            mcp_servers_key: vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string(),
            ],
            ..test_platform("x")
        };
        assert_eq!(primary_servers_key(&p), "first");
    }

    // ── Edge: server_presence_icon exact output ───────────────────────────

    #[test]
    fn server_presence_icon_true_exact() {
        assert_eq!(server_presence_icon(true), "\x1b[32m ✓\x1b[0m");
    }

    #[test]
    fn server_presence_icon_false_exact() {
        assert_eq!(server_presence_icon(false), "\x1b[31m ✗\x1b[0m");
    }
}
