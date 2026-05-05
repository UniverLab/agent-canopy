use crate::setup_module::config_manip::{
    remove_json_key, upsert_json_key, upsert_toml_array, upsert_toml_key,
};
use crate::setup_module::models::{
    load_mcp_fs_root, resolve_config_path, save_mcp_fs_root, CanonicalServers, Platform,
};
use anyhow::Result;
use std::io::{self, Write};
use std::path::Path;

type JsonMap = serde_json::Map<String, serde_json::Value>;

// ── Parsing & normalization ──────────────────────────────────────────────────

/// Replace `{filesystem_dir}` and `{home}` placeholders in a JSON value tree.
fn substitute_placeholders(value: &mut serde_json::Value, home: &str, fs_dir: &str) {
    match value {
        serde_json::Value::String(s) if s.contains("{filesystem_dir}") || s.contains("{home}") => {
            *s = s
                .replace("{filesystem_dir}", fs_dir)
                .replace("{home}", home);
        }
        serde_json::Value::String(_) => {}
        serde_json::Value::Array(arr) => {
            for item in arr {
                substitute_placeholders(item, home, fs_dir);
            }
        }
        serde_json::Value::Object(map) => {
            for val in map.values_mut() {
                substitute_placeholders(val, home, fs_dir);
            }
        }
        _ => {}
    }
}

fn clone_object_entries(obj: &JsonMap) -> JsonMap {
    obj.iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn clone_object_entries_except(obj: &JsonMap, excluded: &[&str]) -> JsonMap {
    obj.iter()
        .filter(|(key, _)| !excluded.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Build the initial field map applying the platform's command_format rule.
fn apply_command_format(obj: &JsonMap, command_format: &str) -> JsonMap {
    if command_format != "merged" {
        return clone_object_entries(obj);
    }

    let Some(command) = obj.get("command").and_then(serde_json::Value::as_str) else {
        return clone_object_entries(obj);
    };
    let Some(args) = obj.get("args") else {
        return clone_object_entries(obj);
    };

    let mut adapted = clone_object_entries_except(obj, &["command", "args"]);
    let mut merged = vec![serde_json::Value::String(command.to_string())];
    if let Some(args) = args.as_array() {
        merged.extend(args.iter().cloned());
    }
    adapted.insert("command".to_string(), serde_json::Value::Array(merged));
    adapted
}

fn rename_mapped_fields(
    adapted: JsonMap,
    fields_mapping: &std::collections::HashMap<String, String>,
) -> JsonMap {
    adapted
        .into_iter()
        .map(|(key, value)| {
            let target_key = fields_mapping.get(&key).cloned().unwrap_or(key);
            (target_key, value)
        })
        .collect()
}

// ── Validation & resolution ─────────────────────────────────────────────────

/// Infer server type index from canonical fields.
/// Returns `0` for url-based (http/remote) or `1` for command-based (stdio/local).
fn infer_server_type_index(config: &JsonMap) -> Option<usize> {
    if config.contains_key("url") {
        return Some(0);
    }
    if config.contains_key("command") {
        return Some(1);
    }
    None
}

fn resolve_required_field_value(
    allowed: &[String],
    type_idx: Option<usize>,
    field_exists: bool,
) -> Option<&str> {
    if let Some(idx) = type_idx {
        return allowed.get(idx).map(String::as_str);
    }
    if field_exists {
        return None;
    }
    allowed.first().map(String::as_str)
}

fn platform_servers_root_key(platform: &Platform) -> &str {
    platform
        .mcp_servers_key
        .first()
        .map(String::as_str)
        .unwrap_or("mcpServers")
}

fn is_toml_platform(platform: &Platform) -> bool {
    platform.config_format.as_deref() == Some("toml")
}

// ── Adaptation ───────────────────────────────────────────────────────────────

fn apply_required_fields(
    adapted: &mut JsonMap,
    required_fields: &std::collections::HashMap<String, Vec<String>>,
) {
    let type_idx = infer_server_type_index(adapted);
    for (field, allowed) in required_fields {
        let Some(value) =
            resolve_required_field_value(allowed, type_idx, adapted.contains_key(field))
        else {
            continue;
        };
        adapted.insert(field.clone(), serde_json::Value::String(value.to_string()));
    }
}

fn merge_json_object(target: &mut JsonMap, source: &JsonMap) {
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

fn merge_server_extras(adapted: &mut JsonMap, platform: &Platform, server_name: &str) {
    let Some(extras) = platform.server_extras.get(server_name) else {
        return;
    };
    let Some(extras_obj) = extras.as_object() else {
        return;
    };
    merge_json_object(adapted, extras_obj);
}

fn strip_unsupported_keys(adapted: &mut JsonMap, unsupported_keys: &[String]) {
    for key in unsupported_keys {
        adapted.remove(key);
    }
}

/// Translate a canonical server config to a target platform's format.
///
/// Applies in order:
/// 1. `command_format` — merge `command` + `args` into single array if "merged"
/// 2. `fields_mapping` — rename fields (e.g. `env` → `environment`)
/// 3. `required_fields` — inject missing required fields with default values
/// 4. `server_extras` — merge per-server platform-specific fields
/// 5. `unsupported_keys` — strip fields the platform doesn't support
pub fn adapt_config(
    config: &serde_json::Value,
    platform: &Platform,
    server_name: &str,
) -> serde_json::Value {
    let Some(obj) = config.as_object() else {
        return config.clone();
    };

    let mut adapted = apply_command_format(obj, &platform.command_format);
    adapted = rename_mapped_fields(adapted, &platform.fields_mapping);
    apply_required_fields(&mut adapted, &platform.required_fields);
    merge_server_extras(&mut adapted, platform, server_name);
    strip_unsupported_keys(&mut adapted, &platform.unsupported_keys);

    serde_json::Value::Object(adapted)
}

/// Interactive directory browser using `inquire::Select`.
/// Lets the user navigate the filesystem and select a directory.
/// Interactive directory picker using arrow-key navigation.
///
/// Keys: ↑↓ navigate  →  enter directory  ←  go up  Enter  confirm  Esc  cancel
pub(crate) fn browse_directory(start_dir: &str) -> String {
    use ratatui::crossterm::event::{read, Event, KeyEventKind};
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    let mut current = std::path::PathBuf::from(start_dir);
    if !current.is_dir() {
        current = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
    }

    let mut cursor: usize = 0;
    let mut filter = String::new();
    let visible: usize = 10;
    let total_rows = 3 + visible + 1;

    let _ = enable_raw_mode();
    for _ in 0..total_rows {
        print!("\r\n");
    }

    loop {
        let subdirs = filter_subdirs_from(&current, &filter);
        adjust_cursor_bounds(&mut cursor, subdirs.len());
        let scroll = calculate_scroll(cursor, visible);
        let has_above = scroll > 0;
        let has_below = !subdirs.is_empty() && scroll + visible < subdirs.len();

        render_browse_display(
            total_rows, &current, &subdirs, cursor, scroll, visible, &filter, has_above, has_below,
        );
        let _ = io::stdout().flush();

        if let Ok(Event::Key(k)) = read() {
            if k.kind == KeyEventKind::Press {
                match handle_browse_key(k.code, &subdirs, &mut cursor, &mut current, &mut filter) {
                    BrowseAction::Confirm => {
                        let _ = disable_raw_mode();
                        print!("\r\n");
                        let _ = io::stdout().flush();
                        return current.to_string_lossy().to_string();
                    }
                    BrowseAction::Cancel => {
                        let _ = disable_raw_mode();
                        print!("\r\n");
                        let _ = io::stdout().flush();
                        return start_dir.to_string();
                    }
                    BrowseAction::Continue => {}
                }
            }
        }
    }
}

fn filter_subdirs_from(current: &std::path::Path, filter: &str) -> Vec<String> {
    let all = browse_list_subdirs(current);
    if filter.is_empty() {
        return all;
    }
    let f = filter.to_lowercase();
    all.into_iter()
        .filter(|name| name.to_lowercase().contains(&f))
        .collect()
}

fn adjust_cursor_bounds(cursor: &mut usize, len: usize) {
    if len > 0 && *cursor >= len {
        *cursor = len - 1;
    }
}

fn calculate_scroll(cursor: usize, visible: usize) -> usize {
    if cursor >= visible {
        cursor - visible + 1
    } else {
        0
    }
}

#[allow(clippy::too_many_arguments)]
fn render_browse_display(
    total_rows: usize,
    current: &std::path::Path,
    subdirs: &[String],
    cursor: usize,
    scroll: usize,
    visible: usize,
    filter: &str,
    has_above: bool,
    has_below: bool,
) {
    print!("\x1b[{total_rows}A");
    print!("\r\x1b[2K  \x1b[36m»\x1b[0m  {}\r\n", current.display());
    print!(
        "\r\x1b[2K  \x1b[90m↑↓ navigate  → enter  ← back  Enter confirm  Esc cancel  type to filter\x1b[0m\r\n"
    );

    if filter.is_empty() {
        print!("\r\x1b[2K  \x1b[90mfilter: _\x1b[0m\r\n");
    } else {
        print!(
            "\r\x1b[2K  filter: \x1b[33m{filter}\x1b[0m \x1b[90m(Backspace to clear)\x1b[0m\r\n"
        );
    }

    render_subdirs_section(subdirs, cursor, scroll, visible, filter);
    render_pagination_line(subdirs, cursor, has_above, has_below);
}

fn render_subdirs_section(
    subdirs: &[String],
    cursor: usize,
    scroll: usize,
    visible: usize,
    filter: &str,
) {
    if subdirs.is_empty() {
        let msg = if filter.is_empty() {
            "(empty — Enter to confirm, ← to go up)"
        } else {
            "(no matches for \"{filter}\")"
        };
        print!("\r\x1b[2K  \x1b[90m{msg}\x1b[0m\r\n");
        for _ in 1..visible {
            print!("\r\x1b[2K\r\n");
        }
    } else {
        let mut drawn = 0;
        for (i, name) in subdirs.iter().enumerate().skip(scroll).take(visible) {
            if i == cursor {
                print!("\r\x1b[2K  \x1b[1;32m▶\x1b[0m \x1b[7m {name} \x1b[0m\r\n");
            } else {
                print!("\r\x1b[2K    {name}\r\n");
            }
            drawn += 1;
        }
        for _ in drawn..visible {
            print!("\r\x1b[2K\r\n");
        }
    }
}

fn render_pagination_line(subdirs: &[String], cursor: usize, has_above: bool, has_below: bool) {
    if subdirs.is_empty() {
        print!("\r\x1b[2K  \x1b[90m0 items\x1b[0m\r\n");
    } else {
        let up = if has_above { "↑ " } else { "  " };
        let dn = if has_below { " ↓" } else { "  " };
        print!(
            "\r\x1b[2K  \x1b[90m{up}{}/{}{dn}\x1b[0m\r\n",
            cursor + 1,
            subdirs.len()
        );
    }
}

enum BrowseAction {
    Confirm,
    Cancel,
    Continue,
}

fn handle_browse_key(
    code: ratatui::crossterm::event::KeyCode,
    subdirs: &[String],
    cursor: &mut usize,
    current: &mut std::path::PathBuf,
    filter: &mut String,
) -> BrowseAction {
    use ratatui::crossterm::event::KeyCode;
    match code {
        KeyCode::Enter => BrowseAction::Confirm,
        KeyCode::Esc => BrowseAction::Cancel,
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
            BrowseAction::Continue
        }
        KeyCode::Down if !subdirs.is_empty() && *cursor + 1 < subdirs.len() => {
            *cursor += 1;
            BrowseAction::Continue
        }
        KeyCode::Right | KeyCode::Char('l') if filter.is_empty() => {
            if let Some(name) = subdirs.get(*cursor) {
                *current = current.join(name);
                *cursor = 0;
            }
            BrowseAction::Continue
        }
        KeyCode::Left | KeyCode::Char('h') if filter.is_empty() => {
            if let Some(parent) = current.parent() {
                *current = parent.to_path_buf();
                *cursor = 0;
            }
            BrowseAction::Continue
        }
        // → with filter: enter the highlighted match then clear filter
        KeyCode::Right if !filter.is_empty() => {
            if let Some(name) = subdirs.get(*cursor) {
                *current = current.join(name);
                *cursor = 0;
                filter.clear();
            }
            BrowseAction::Continue
        }
        KeyCode::Backspace => {
            filter.pop();
            *cursor = 0;
            BrowseAction::Continue
        }
        KeyCode::Char(c) => {
            filter.push(c);
            *cursor = 0;
            BrowseAction::Continue
        }
        _ => BrowseAction::Continue,
    }
}

fn browse_list_subdirs(path: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                || (e.file_type().map(|t| t.is_symlink()).unwrap_or(false) && e.path().is_dir())
        })
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                None
            } else {
                Some(name)
            }
        })
        .collect();
    dirs.sort();
    dirs
}

/// Extract all MCP server configs from the selected platforms.
pub(crate) fn extract_all_mcp_configs(
    home: &Path,
    selected: &[&Platform],
) -> Vec<crate::config::PlatformMcpConfig> {
    let mut configs = Vec::new();
    for p in selected {
        let config_path = resolve_config_path(home, &p.config_path);
        if !config_path.exists() {
            configs.push(crate::config::PlatformMcpConfig {
                platform: p.name.clone(),
                config_path: config_path.to_string_lossy().to_string(),
                servers: Vec::new(),
            });
            continue;
        }
        match crate::config::McpConfigRegistry::extract_from_platform(
            &p.name,
            &config_path,
            &p.mcp_servers_key,
        ) {
            Ok(cfg) => configs.push(cfg),
            Err(_) => configs.push(crate::config::PlatformMcpConfig {
                platform: p.name.clone(),
                config_path: config_path.to_string_lossy().to_string(),
                servers: Vec::new(),
            }),
        }
    }
    configs
}

pub(crate) fn print_mcp_matrix(all_configs: &[crate::config::PlatformMcpConfig]) {
    use std::collections::BTreeSet;

    if all_configs.is_empty() {
        return;
    }

    let mut all_servers: BTreeSet<String> = all_configs
        .iter()
        .flat_map(|c| c.servers.iter().map(|s| s.name.clone()))
        .collect();
    // Always show our 4 core servers in the matrix
    for s in &["canopy", "fetch", "filesystem"] {
        all_servers.insert(s.to_string());
    }

    let server_col = 20usize;
    let cell_col = 3usize;
    let total_width = 2 + server_col + 1 + (all_configs.len() * (cell_col + 1));

    println!("  MCP overview:");
    println!(
        "  {:<server_col$} {}",
        "Server",
        (1..=all_configs.len())
            .map(|i| format!("{:>cell_col$}", i, cell_col = cell_col))
            .collect::<Vec<_>>()
            .join(" "),
        server_col = server_col
    );
    println!("  {:─<width$}", "", width = total_width.max(34));
    for server_name in &all_servers {
        let mut row = format!("  {:<server_col$}", server_name, server_col = server_col);
        for config in all_configs {
            let has = config.servers.iter().any(|s| s.name == *server_name);
            let icon = if has {
                "\x1b[32m✓\x1b[0m"
            } else {
                "\x1b[31m✗\x1b[0m"
            };
            // Manual padding: ANSI codes break format width, so pad explicitly
            row.push_str(&format!(" {}{}", " ".repeat(cell_col - 1), icon));
        }
        println!("{}", row);
    }
    println!();
    println!("  Platforms:");
    for (idx, cfg) in all_configs.iter().enumerate() {
        println!("    {:>2}: {}", idx + 1, cfg.platform);
    }
}

pub(crate) fn clear_wizard_screen() -> Result<()> {
    print!("\x1b[2J\x1b[H");
    io::stdout().flush()?;
    Ok(())
}

// ── Output ───────────────────────────────────────────────────────────────────

fn apply_upsert_to_platform(
    platform: &Platform,
    config_path: &Path,
    server_name: &str,
    config: &serde_json::Value,
) -> Result<bool> {
    if !is_toml_platform(platform) {
        let mut key_refs: Vec<_> = platform
            .mcp_servers_key
            .iter()
            .map(String::as_str)
            .collect();
        key_refs.push(server_name);
        return upsert_json_key(config_path, &key_refs, config);
    }

    if platform.toml_array_format {
        return upsert_toml_array(
            config_path,
            &platform.mcp_servers_key.join("."),
            server_name,
            config,
        );
    }

    upsert_toml_key(
        config_path,
        platform_servers_root_key(platform),
        server_name,
        config,
    )
}

fn initial_platform_config(platform: &Platform) -> String {
    if is_toml_platform(platform) {
        return String::new();
    }

    format!("{{\"{}\": {{}}}}\n", platform_servers_root_key(platform))
}

fn ensure_platform_config_exists(config_path: &Path, platform: &Platform) {
    if config_path.exists() {
        return;
    }

    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(config_path, initial_platform_config(platform));
}

fn remove_deprecated_platform_keys(config_path: &Path, platform: &Platform) {
    if is_toml_platform(platform) {
        return;
    }

    let servers_parent = platform_servers_root_key(platform);
    for old_key in &platform.deprecated_keys {
        let _ = remove_json_key(config_path, servers_parent, old_key);
    }
}

fn write_server_config(
    platform: &Platform,
    config_path: &Path,
    home: &str,
    fs_dir: &str,
    server_name: &str,
    template: &serde_json::Value,
) {
    let mut config = template.clone();
    substitute_placeholders(&mut config, home, fs_dir);

    let adapted = adapt_config(&config, platform, server_name);
    if let Err(error) = apply_upsert_to_platform(platform, config_path, server_name, &adapted) {
        eprintln!(
            "  \x1b[33m⚠\x1b[0m  Failed to write {server_name} for {}: {error}",
            platform.name
        );
    }
}

fn write_canonical_servers(
    platform: &Platform,
    config_path: &Path,
    canonical: &CanonicalServers,
    home: &str,
    fs_dir: &str,
) {
    for (server_name, template) in &canonical.servers {
        write_server_config(platform, config_path, home, fs_dir, server_name, template);
    }
}

fn resolve_filesystem_root(home: &Path, canonical: &CanonicalServers) -> String {
    if !canonical.servers.contains_key("filesystem") {
        return load_mcp_fs_root(home);
    }

    let current_fs = load_mcp_fs_root(home);
    println!();
    println!("  \x1b[36mFilesystem MCP root directory\x1b[0m");
    println!("  Agents will have read/write access to everything inside this directory.");
    println!("  Choose a project folder or workspace root.");
    println!("  Current: \x1b[33m{}\x1b[0m", current_fs);
    println!();

    let chosen = browse_directory(&current_fs);
    save_mcp_fs_root(home, &chosen);
    chosen
}

/// Install/update canopy + recommended MCP servers on all selected platforms.
/// Translates canonical server definitions using each platform's rules.
pub(crate) fn run_install_our_servers(
    home: &Path,
    selected: &[&Platform],
    canonical: &CanonicalServers,
) -> Result<()> {
    let fs_dir = resolve_filesystem_root(home, canonical);
    let home_str = home.to_string_lossy().to_string();

    for platform in selected {
        let config_path = resolve_config_path(home, &platform.config_path);
        ensure_platform_config_exists(&config_path, platform);
        remove_deprecated_platform_keys(&config_path, platform);
        write_canonical_servers(platform, &config_path, canonical, &home_str, &fs_dir);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{adapt_config, apply_command_format};
    use crate::setup_module::models::Platform;

    fn test_platform() -> Platform {
        Platform {
            name: "test".to_string(),
            config_path: "config.json".to_string(),
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
            cli: None,
        }
    }

    #[test]
    fn apply_command_format_merges_command_and_args_when_requested() {
        let config = serde_json::json!({
            "command": "uvx",
            "args": ["--from", "pkg"],
            "env": {"A": "1"}
        });

        let adapted = apply_command_format(config.as_object().unwrap(), "merged");

        assert_eq!(
            adapted.get("command"),
            Some(&serde_json::json!(["uvx", "--from", "pkg"]))
        );
        assert_eq!(adapted.get("env"), Some(&serde_json::json!({"A": "1"})));
        assert!(!adapted.contains_key("args"));
    }

    #[test]
    fn adapt_config_applies_platform_transformations_in_order() {
        let mut platform = test_platform();
        platform.command_format = "merged".to_string();
        platform
            .fields_mapping
            .insert("env".to_string(), "environment".to_string());
        platform.required_fields.insert(
            "type".to_string(),
            vec!["http".to_string(), "stdio".to_string()],
        );
        platform.unsupported_keys.push("remove_me".to_string());
        platform
            .server_extras
            .insert("canopy".to_string(), serde_json::json!({"tools": ["*"]}));

        let adapted = adapt_config(
            &serde_json::json!({
                "command": "uvx",
                "args": ["canopy"],
                "env": {"A": "1"},
                "remove_me": true
            }),
            &platform,
            "canopy",
        );

        assert_eq!(
            adapted,
            serde_json::json!({
                "command": ["uvx", "canopy"],
                "environment": {"A": "1"},
                "type": "stdio",
                "tools": ["*"]
            })
        );
    }

    #[test]
    fn adapt_config_keeps_existing_required_field_when_type_is_unknown() {
        let mut platform = test_platform();
        platform
            .required_fields
            .insert("type".to_string(), vec!["http".to_string()]);

        let adapted = adapt_config(
            &serde_json::json!({
                "name": "existing",
                "type": "custom"
            }),
            &platform,
            "canopy",
        );

        assert_eq!(
            adapted,
            serde_json::json!({
                "name": "existing",
                "type": "custom"
            })
        );
    }
}
