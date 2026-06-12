use crate::setup_module::config_manip::{
    remove_json_key, remove_toml_array_entry_str, remove_toml_key_section_str, upsert_json_key,
    upsert_toml_array, upsert_toml_key,
};
use crate::setup_module::models::Platform;
use anyhow::Result;
use std::path::Path;

// ── Public thin wrappers for mcp_wizard ────────────────────────────────────

/// Public wrapper for [`upsert_json_key`].
pub fn upsert_json_key_pub(path: &Path, keys: &[&str], value: &serde_json::Value) -> Result<bool> {
    upsert_json_key(path, keys, value)
}

/// Public wrapper for [`upsert_toml_key`].
pub fn upsert_toml_key_pub(
    path: &Path,
    section: &str,
    entry_key: &str,
    value: &serde_json::Value,
) -> Result<bool> {
    upsert_toml_key(path, section, entry_key, value)
}

/// Public wrapper for [`upsert_toml_array`].
pub fn upsert_toml_array_pub(
    path: &Path,
    section: &str,
    entry_key: &str,
    value: &serde_json::Value,
) -> Result<bool> {
    upsert_toml_array(path, section, entry_key, value)
}

/// Public wrapper for [`remove_json_key`].
pub fn remove_json_key_pub(path: &Path, parent_key: &str, key: &str) -> Result<bool> {
    remove_json_key(path, parent_key, key)
}

/// Remove a server entry from a TOML platform config, handling both key-table
/// and array-of-tables formats.
pub fn remove_toml_server_pub(platform: &Platform, path: &Path, server_name: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(path)?;

    let updated = if platform.toml_array_format {
        let section = platform.mcp_servers_key.join(".");
        let array_header = format!("[[{section}]]");
        let name_line = format!("name = \"{server_name}\"");
        if !content.contains(&name_line) {
            return Ok(false);
        }
        remove_toml_array_entry_str(&content, &array_header, &name_line)
    } else {
        let section = platform
            .mcp_servers_key
            .first()
            .cloned()
            .unwrap_or_else(|| "mcpServers".to_string());
        let table_header = format!("[{section}.{server_name}]");
        if !content.contains(&table_header) {
            return Ok(false);
        }
        remove_toml_key_section_str(&content, &table_header)
    };

    if updated == content {
        return Ok(false);
    }

    std::fs::write(path, updated)?;
    Ok(true)
}
