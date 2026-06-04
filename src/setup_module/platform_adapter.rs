use crate::setup_module::config_manip::{
    remove_json_key, upsert_json_key, upsert_toml_array, upsert_toml_key,
};
use crate::setup_module::dir_browser::browse_directory;
use crate::setup_module::models::{
    load_mcp_fs_root, resolve_config_path, save_mcp_fs_root, CanonicalServers, Platform,
};
use anyhow::Result;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

type JsonMap = serde_json::Map<String, serde_json::Value>;

// ── Parsing & normalization ──────────────────────────────────────────────────

fn substitute_placeholders(value: &mut serde_json::Value, home: &str, fs_dir: &str) {
    let serde_json::Value::String(content) = value else {
        substitute_in_container(value, home, fs_dir);
        return;
    };

    if !content.contains("{filesystem_dir}") && !content.contains("{home}") {
        return;
    }
    *content = substitute_string_placeholders(content, home, fs_dir);
}

fn substitute_in_container(value: &mut serde_json::Value, home: &str, fs_dir: &str) {
    match value {
        serde_json::Value::Array(arr) => {
            arr.iter_mut()
                .for_each(|item| substitute_placeholders(item, home, fs_dir));
        }
        serde_json::Value::Object(map) => {
            map.values_mut()
                .for_each(|v| substitute_placeholders(v, home, fs_dir));
        }
        _ => {}
    }
}

fn substitute_string_placeholders(content: &str, home: &str, fs_dir: &str) -> String {
    content
        .replace("{filesystem_dir}", fs_dir)
        .replace("{home}", home)
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

fn enforce_canopy_bridge_transport(adapted: &mut JsonMap, platform: &Platform, server_name: &str) {
    if server_name != "canopy" {
        return;
    }

    adapted.remove("url");
    adapted.remove("headers");

    if platform.command_format == "merged" {
        adapted.insert(
            "command".to_string(),
            serde_json::json!(["canopy", "bridge"]),
        );
        adapted.remove("args");
        return;
    }

    adapted.insert(
        "command".to_string(),
        serde_json::Value::String("canopy".to_string()),
    );
    let needs_bridge = adapted
        .get("args")
        .and_then(serde_json::Value::as_array)
        .map(|arr| !arr.iter().any(|v| v.as_str() == Some("bridge")))
        .unwrap_or(true);
    if needs_bridge {
        adapted.insert("args".to_string(), serde_json::json!(["bridge"]));
    }
}

/// Translate a canonical server config to a target platform's format.
///
/// Applies in order:
/// 1. `command_format` — merge `command` + `args` into single array if "merged"
/// 2. `fields_mapping` — rename fields (e.g. `env` → `environment`)
/// 3. `canopy bridge` enforcement — migrate canopy server to stdio sidecar
/// 4. `required_fields` — inject missing required fields with default values
/// 5. `server_extras` — merge per-server platform-specific fields
/// 6. `unsupported_keys` — strip fields the platform doesn't support
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
    enforce_canopy_bridge_transport(&mut adapted, platform, server_name);
    apply_required_fields(&mut adapted, &platform.required_fields);
    merge_server_extras(&mut adapted, platform, server_name);
    strip_unsupported_keys(&mut adapted, &platform.unsupported_keys);

    serde_json::Value::Object(adapted)
}

// ── MCP config extraction & display ──────────────────────────────────────────

fn empty_platform_config(
    platform: &crate::setup_module::models::Platform,
    config_path: String,
) -> crate::config::PlatformMcpConfig {
    crate::config::PlatformMcpConfig {
        platform: platform.name.clone(),
        config_path,
        servers: Vec::new(),
    }
}

pub(crate) fn extract_all_mcp_configs(
    home: &Path,
    selected: &[&Platform],
) -> Vec<crate::config::PlatformMcpConfig> {
    selected
        .iter()
        .map(|p| {
            let config_path = resolve_config_path(home, &p.config_path);
            let path_str = config_path.to_string_lossy().to_string();

            if !config_path.exists() {
                return empty_platform_config(p, path_str);
            }

            match crate::config::McpConfigRegistry::extract_from_platform(
                &p.name,
                &config_path,
                &p.mcp_servers_key,
            ) {
                Ok(cfg) => cfg,
                Err(_) => empty_platform_config(p, path_str),
            }
        })
        .collect()
}

fn collect_all_server_names(all_configs: &[crate::config::PlatformMcpConfig]) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = all_configs
        .iter()
        .flat_map(|c| c.servers.iter().map(|s| s.name.clone()))
        .collect();
    for s in &["canopy", "fetch", "filesystem"] {
        names.insert(s.to_string());
    }
    names
}

fn format_matrix_header(config_count: usize) -> String {
    let server_col = 20usize;
    let cell_col = 3usize;
    format!(
        " {:<server_col$} {}",
        "Server",
        (1..=config_count)
            .map(|i| format!("{:>cell_col$}", i, cell_col = cell_col))
            .collect::<Vec<_>>()
            .join(" "),
        server_col = server_col
    )
}

fn matrix_separator_width(config_count: usize) -> usize {
    let server_col = 20usize;
    let cell_col = 3usize;
    2 + server_col + 1 + (config_count * (cell_col + 1))
}

fn format_matrix_row(
    server_name: &str,
    all_configs: &[crate::config::PlatformMcpConfig],
) -> String {
    let server_col = 20usize;
    let cell_col = 3usize;
    let mut row = format!(" {:<server_col$}", server_name, server_col = server_col);
    for config in all_configs {
        let has = config.servers.iter().any(|s| s.name == server_name);
        let icon = if has {
            "\x1b[32m✓\x1b[0m"
        } else {
            "\x1b[31m✗\x1b[0m"
        };
        row.push_str(&format!(" {}{}", " ".repeat(cell_col - 1), icon));
    }
    row
}

fn print_platform_list(all_configs: &[crate::config::PlatformMcpConfig]) {
    println!(" Platforms:");
    for (idx, cfg) in all_configs.iter().enumerate() {
        println!(" {:>2}: {}", idx + 1, cfg.platform);
    }
}

pub(crate) fn print_mcp_matrix(all_configs: &[crate::config::PlatformMcpConfig]) {
    if all_configs.is_empty() {
        return;
    }

    let all_servers = collect_all_server_names(all_configs);
    let total_width = matrix_separator_width(all_configs.len());

    println!(" MCP overview:");
    println!("{}", format_matrix_header(all_configs.len()));
    println!(" {:─<width$}", "", width = total_width.max(34));

    for server_name in &all_servers {
        println!("{}", format_matrix_row(server_name, all_configs));
    }

    println!();
    print_platform_list(all_configs);
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
            " \x1b[33m⚠\x1b[0m Failed to write {server_name} for {}: {error}",
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
    println!(" \x1b[36mFilesystem MCP root directory\x1b[0m");
    println!(" Agents will have read/write access to everything inside this directory.");
    println!(" Choose a project folder or workspace root.");
    println!(" Current: \x1b[33m{}\x1b[0m", current_fs);
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
        platform.server_extras.insert(
            "filesystem".to_string(),
            serde_json::json!({"tools": ["*"]}),
        );

        let adapted = adapt_config(
            &serde_json::json!({
                "command": "uvx",
                "args": ["canopy"],
                "env": {"A": "1"},
                "remove_me": true
            }),
            &platform,
            "filesystem",
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
            "fetch",
        );

        assert_eq!(
            adapted,
            serde_json::json!({
                "name": "existing",
                "type": "custom"
            })
        );
    }

    #[test]
    fn adapt_config_migrates_canopy_http_to_bridge_command() {
        let mut platform = test_platform();
        platform.name = "copilot".to_string();
        platform.required_fields.insert(
            "type".to_string(),
            vec!["http".to_string(), "stdio".to_string()],
        );

        let adapted = adapt_config(
            &serde_json::json!({
                "url": "http://localhost:7755/mcp",
                "headers": {"x-any": "value"},
                "tools": ["*"]
            }),
            &platform,
            "canopy",
        );

        assert_eq!(
            adapted,
            serde_json::json!({
                "command": "canopy",
                "args": ["bridge"],
                "tools": ["*"],
                "type": "stdio"
            })
        );
    }

    #[test]
    fn adapt_config_forces_canopy_bridge_for_merged_platforms() {
        let mut platform = test_platform();
        platform.name = "claude".to_string();
        platform.command_format = "merged".to_string();
        platform.required_fields.insert(
            "type".to_string(),
            vec!["http".to_string(), "stdio".to_string()],
        );

        let adapted = adapt_config(
            &serde_json::json!({
                "command": "ignored",
                "args": ["ignored-too"],
                "enabled": true
            }),
            &platform,
            "canopy",
        );

        assert_eq!(
            adapted,
            serde_json::json!({
                "command": ["canopy", "bridge"],
                "enabled": true,
                "type": "stdio"
            })
        );
    }
}
