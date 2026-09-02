use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::setup_module::models::resolve_config_path;
use crate::setup_module::models::Platform;

pub struct SynthesizedConfig {
    pub path: PathBuf,
    pub mcp_surface: String,
}

pub fn synthesize_mcp_config(
    platform: &Platform,
    home: &Path,
    servers: &[&str],
) -> Result<SynthesizedConfig> {
    let config_dir = home.join(".canopy").join("subagent_configs");
    std::fs::create_dir_all(&config_dir)?;

    let real_config_path = resolve_config_path(home, &platform.config_path);
    let is_toml = platform.config_format.as_deref() == Some("toml");
    let ext = if is_toml { "toml" } else { "json" };
    let file_name = format!("{}-{}.{}", platform.name, uuid::Uuid::new_v4(), ext);
    let output_path = config_dir.join(file_name);

    let (config_content, surface) = if !real_config_path.exists() {
        let surface = build_surface_description(&[]);
        let content = if is_toml {
            build_empty_toml_config(platform)
        } else {
            build_empty_json_config(platform)
        };
        (content, surface)
    } else {
        let raw = std::fs::read_to_string(&real_config_path)?;
        if is_toml {
            synthesize_toml_config(&raw, platform, servers)?
        } else {
            synthesize_json_config(&raw, platform, servers)?
        }
    };

    std::fs::write(&output_path, config_content)?;

    Ok(SynthesizedConfig {
        path: output_path,
        mcp_surface: surface,
    })
}

fn synthesize_json_config(
    raw: &str,
    platform: &Platform,
    servers: &[&str],
) -> Result<(String, String)> {
    let clean = crate::setup_module::strip_jsonc_comments(raw);
    let root: serde_json::Value =
        serde_json::from_str(&clean).context("Failed to parse platform config")?;

    let servers_key: Vec<&str> = platform
        .mcp_servers_key
        .iter()
        .map(|s| s.as_str())
        .collect();

    let existing_servers = traverse_to_object(&root, &servers_key);

    let mut filtered = serde_json::Map::new();
    let mut included_names = Vec::new();

    for &name in servers {
        if let Some(server_val) = existing_servers.and_then(|m| m.get(name)) {
            filtered.insert(name.to_string(), server_val.clone());
            included_names.push(name.to_string());
        }
    }

    let mut new_root: serde_json::Value = serde_json::Value::Object(serde_json::Map::new());
    let mut current = &mut new_root;
    for (i, key) in servers_key.iter().enumerate() {
        if i == servers_key.len() - 1 {
            if let serde_json::Value::Object(map) = current {
                map.insert(key.to_string(), serde_json::Value::Object(filtered.clone()));
            }
        } else {
            if let serde_json::Value::Object(map) = current {
                map.insert(
                    key.to_string(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
            }
            current = current.get_mut(*key).unwrap();
        }
    }

    let surface = build_surface_description(
        &included_names
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>(),
    );

    Ok((serde_json::to_string_pretty(&new_root)? + "\n", surface))
}

fn synthesize_toml_config(
    raw: &str,
    platform: &Platform,
    servers: &[&str],
) -> Result<(String, String)> {
    let mut doc: toml::Value = toml::from_str(raw).context("Failed to parse TOML config")?;

    let servers_key = &platform.mcp_servers_key;
    let mut included_names = Vec::new();

    if platform.toml_array_format {
        if let Some(arr) = get_toml_array_mut(&mut doc, servers_key) {
            let kept: Vec<toml::Value> = arr
                .iter()
                .filter(|entry| {
                    let name = entry
                        .as_table()
                        .and_then(|t| t.get("name"))
                        .and_then(|v| v.as_str());
                    let keep = match name {
                        Some(n) => servers.contains(&n),
                        None => false,
                    };
                    if keep {
                        if let Some(n) = name {
                            included_names.push(n.to_string());
                        }
                    }
                    keep
                })
                .cloned()
                .collect();
            *arr = kept;
        }
    } else if let Some(table) = get_toml_table_mut(&mut doc, servers_key) {
        let keys_to_remove: Vec<String> = table
            .keys()
            .filter(|k| !servers.contains(&k.as_str()))
            .cloned()
            .collect();
        for k in keys_to_remove {
            table.remove(&k);
        }
        for k in table.keys() {
            included_names.push(k.clone());
        }
    }

    let surface = build_surface_description(
        &included_names
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>(),
    );

    Ok((toml::to_string_pretty(&doc)?, surface))
}

fn build_empty_json_config(platform: &Platform) -> String {
    let servers_key: Vec<&str> = platform
        .mcp_servers_key
        .iter()
        .map(|s| s.as_str())
        .collect();

    let filtered = serde_json::Map::new();

    if servers_key.is_empty() {
        return "{}\n".to_string();
    }

    let mut new_root: serde_json::Value = serde_json::Value::Object(serde_json::Map::new());
    let mut current = &mut new_root;
    for (i, key) in servers_key.iter().enumerate() {
        if i == servers_key.len() - 1 {
            if let serde_json::Value::Object(map) = current {
                map.insert(key.to_string(), serde_json::Value::Object(filtered.clone()));
            }
        } else {
            if let serde_json::Value::Object(map) = current {
                map.insert(
                    key.to_string(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
            }
            current = current.get_mut(*key).unwrap();
        }
    }

    serde_json::to_string_pretty(&new_root).unwrap_or_default() + "\n"
}

fn build_empty_toml_config(platform: &Platform) -> String {
    let servers_key = &platform.mcp_servers_key;

    if platform.toml_array_format {
        let mut doc: toml::Value = toml::Value::Table(toml::map::Map::new());
        let mut current = &mut doc;
        for (i, key) in servers_key.iter().enumerate() {
            if i == servers_key.len() - 1 {
                if let toml::Value::Table(map) = current {
                    map.insert(key.clone(), toml::Value::Array(Vec::new()));
                }
            } else {
                if let toml::Value::Table(map) = current {
                    map.insert(key.clone(), toml::Value::Table(toml::map::Map::new()));
                }
                current = current.get_mut(key).unwrap();
            }
        }
        toml::to_string_pretty(&doc).unwrap_or_default()
    } else if let Some(first_key) = servers_key.first() {
        let mut doc = toml::map::Map::new();
        doc.insert(first_key.clone(), toml::Value::Table(toml::map::Map::new()));
        toml::to_string_pretty(&toml::Value::Table(doc)).unwrap_or_default()
    } else {
        String::new()
    }
}

fn build_surface_description(included: &[&str]) -> String {
    let names: Vec<&str> = included.to_vec();
    if names.is_empty() {
        "(blind — no MCP servers)".to_string()
    } else {
        names.join(", ")
    }
}

fn traverse_to_object<'a>(
    root: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    let mut current = root;
    for key in keys {
        current = current.get(*key)?;
    }
    current.as_object()
}

fn get_toml_array_mut<'a>(
    root: &'a mut toml::Value,
    keys: &[String],
) -> Option<&'a mut Vec<toml::Value>> {
    let mut current = root;
    for key in keys {
        current = current.get_mut(key)?;
    }
    current.as_array_mut()
}

fn get_toml_table_mut<'a>(
    root: &'a mut toml::Value,
    keys: &[String],
) -> Option<&'a mut toml::map::Map<String, toml::Value>> {
    let mut current = root;
    for key in keys {
        current = current.get_mut(key)?;
    }
    current.as_table_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_platform_json() -> Platform {
        Platform {
            name: "test".to_string(),
            config_path: "test/config.json".to_string(),
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
            cli: None,
        }
    }

    fn test_platform_toml() -> Platform {
        Platform {
            name: "test_toml".to_string(),
            config_path: "test/config.toml".to_string(),
            config_format: Some("toml".to_string()),
            toml_array_format: false,
            command_format: "separate".to_string(),
            mcp_servers_key: vec!["mcp_servers".to_string()],
            deprecated_keys: vec![],
            unsupported_keys: vec![],
            fields_mapping: std::collections::HashMap::new(),
            required_fields: std::collections::HashMap::new(),
            server_extras: std::collections::HashMap::new(),
            skills_dir: None,
            instruction_file: None,
            cli: None,
        }
    }

    #[test]
    fn synthesize_json_config_includes_only_named_servers() {
        let raw = r#"{
  "mcpServers": {
    "fetch": { "command": "fetch" },
    "filesystem": { "command": "fs" },
    "github": { "command": "gh" },
    "canopy": { "command": "canopy" },
    "extra": { "command": "extra" }
  }
}"#;
        let platform = test_platform_json();
        let (content, _surface) =
            synthesize_json_config(raw, &platform, &["fetch", "filesystem"]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let servers = parsed.get("mcpServers").unwrap().as_object().unwrap();
        assert_eq!(servers.len(), 2);
        assert!(servers.contains_key("fetch"));
        assert!(servers.contains_key("filesystem"));
        assert!(!servers.contains_key("github"));
        assert!(!servers.contains_key("canopy"));
        assert!(!servers.contains_key("extra"));
    }

    #[test]
    fn synthesize_json_config_blind_mode() {
        let raw = r#"{
  "mcpServers": {
    "fetch": { "command": "fetch" },
    "filesystem": { "command": "fs" }
  }
}"#;
        let platform = test_platform_json();
        let (content, surface) = synthesize_json_config(raw, &platform, &[]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let servers = parsed.get("mcpServers").unwrap().as_object().unwrap();
        assert_eq!(servers.len(), 0);
        assert!(surface.contains("blind"));
    }

    #[test]
    fn synthesize_json_config_canopy_via_servers_list() {
        let raw = r#"{
  "mcpServers": {
    "fetch": { "command": "fetch" },
    "canopy": { "command": "canopy" }
  }
}"#;
        let platform = test_platform_json();
        let (content, surface) = synthesize_json_config(raw, &platform, &["canopy"]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let servers = parsed.get("mcpServers").unwrap().as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("canopy"));
        assert!(surface.contains("canopy"));
    }

    #[test]
    fn synthesize_toml_config_includes_only_named_servers() {
        let raw = r#"[mcp_servers.fetch]
command = "fetch"

[mcp_servers.filesystem]
command = "fs"

[mcp_servers.github]
command = "gh"
"#;
        let platform = test_platform_toml();
        let (content, _surface) = synthesize_toml_config(raw, &platform, &["fetch"]).unwrap();
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        let servers = parsed.get("mcp_servers").unwrap().as_table().unwrap();
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("fetch"));
        assert!(!servers.contains_key("filesystem"));
        assert!(!servers.contains_key("github"));
    }

    #[test]
    fn synthesize_toml_array_format() {
        let mut platform = test_platform_toml();
        platform.toml_array_format = true;
        let raw = r#"[[mcp_servers]]
name = "fetch"
command = "fetch"

[[mcp_servers]]
name = "filesystem"
command = "fs"

[[mcp_servers]]
name = "canopy"
command = "canopy"
"#;
        let (content, _surface) =
            synthesize_toml_config(raw, &platform, &["fetch", "canopy"]).unwrap();
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        let arr = parsed.get("mcp_servers").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"fetch"));
        assert!(names.contains(&"canopy"));
        assert!(!names.contains(&"filesystem"));
    }

    #[test]
    fn build_surface_description_blind() {
        let desc = build_surface_description(&[]);
        assert!(desc.contains("blind"));
    }

    #[test]
    fn build_surface_description_with_servers() {
        let desc = build_surface_description(&["fetch", "fs", "canopy"]);
        assert!(desc.contains("fetch"));
        assert!(desc.contains("fs"));
        assert!(desc.contains("canopy"));
    }
}
