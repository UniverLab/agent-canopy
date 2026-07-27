pub mod skills;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// MCP Server configuration entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    /// Server name/key (e.g., "canopy", "github", "filesystem")
    pub name: String,
    /// Server configuration (varies by platform format)
    pub config: serde_json::Value,
    /// Whether this server is enabled
    pub enabled: bool,
}

/// Platform MCP configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformMcpConfig {
    /// Platform name (e.g., "kiro", "opencode", "copilot", "qwen")
    pub platform: String,
    /// Path to the config file
    pub config_path: String,
    /// All MCP servers configured for this platform
    pub servers: Vec<McpServerEntry>,
}

/// Registry of all platform MCP configs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfigRegistry {
    /// Version of the config format
    pub version: u32,
    /// All platform configurations
    pub platforms: Vec<PlatformMcpConfig>,
}

impl McpConfigRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            version: 1,
            platforms: Vec::new(),
        }
    }

    /// Extract MCP configs from a platform's config file.
    pub fn extract_from_platform(
        platform_name: &str,
        config_path: &Path,
        servers_key: &[String],
    ) -> Result<PlatformMcpConfig> {
        if !config_path.exists() {
            return Err(anyhow::anyhow!(
                "Config file not found: {}",
                config_path.display()
            ));
        }

        let content = std::fs::read_to_string(config_path)?;

        // Parse file — TOML or JSON depending on extension
        let root: serde_json::Value =
            if config_path.extension().and_then(|e| e.to_str()) == Some("toml") {
                let toml_val: toml::Value =
                    toml::from_str(&content).context("Failed to parse TOML config")?;
                serde_json::to_value(&toml_val).context("Failed to convert TOML to JSON")?
            } else {
                let clean = crate::setup_module::strip_jsonc_comments(&content);
                serde_json::from_str(&clean).context("Failed to parse config file")?
            };

        let mut current = &root;
        for key in servers_key {
            current = current
                .get(key)
                .ok_or_else(|| anyhow::anyhow!("Key '{}' not found in config", key))?;
        }

        let servers = if current.is_array() {
            extract_servers_from_array(current)
        } else {
            extract_servers_from_object(current)
        };

        Ok(PlatformMcpConfig {
            platform: platform_name.to_string(),
            config_path: config_path.to_string_lossy().to_string(),
            servers,
        })
    }

    /// Extract all MCP configs from detected platforms.
    #[allow(dead_code)]
    pub fn extract_all(platforms: &[&crate::setup_module::Platform]) -> Result<Self> {
        let mut registry = Self::new();
        let home = dirs::home_dir().context("No home directory")?;

        for platform in platforms {
            let config_path = home.join(&platform.config_path);
            if !config_path.exists() {
                continue;
            }

            match Self::extract_from_platform(
                &platform.name,
                &home.join(&platform.config_path),
                &platform.mcp_servers_key,
            ) {
                Ok(platform_config) => {
                    registry.platforms.push(platform_config);
                }
                Err(e) => {
                    tracing::warn!("Failed to extract MCPs from {}: {}", platform.name, e);
                }
            }
        }

        Ok(registry)
    }

    /// Get all unique MCP server names across all platforms.
    #[allow(dead_code)]
    pub fn unique_server_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .platforms
            .iter()
            .flat_map(|p| p.servers.iter().map(|s| s.name.as_str()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Get servers that exist in one platform but not another.
    #[allow(dead_code)]
    pub fn server_diff(&self, from: &str, to: &str) -> Vec<&McpServerEntry> {
        let from_servers: Vec<&McpServerEntry> = self
            .platforms
            .iter()
            .find(|p| p.platform == from)
            .map(|p| p.servers.iter().collect::<Vec<_>>())
            .unwrap_or_default();

        let to_server_names: Vec<&str> = self
            .platforms
            .iter()
            .find(|p| p.platform == to)
            .map(|p| {
                p.servers
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        from_servers
            .into_iter()
            .filter(|s| !to_server_names.contains(&s.name.as_str()))
            .collect()
    }

    /// Sync selected servers to target platforms.
    #[allow(dead_code)]
    pub fn sync_servers(
        &self,
        server_names: &[&str],
        target_platforms: &[&str],
    ) -> Result<Vec<String>> {
        let mut synced = Vec::new();

        for platform_name in target_platforms {
            let platform = self
                .platforms
                .iter()
                .find(|p| p.platform == *platform_name)
                .ok_or_else(|| {
                    anyhow::anyhow!("Platform '{}' not found in registry", platform_name)
                })?;

            for server_name in server_names {
                if platform.servers.iter().any(|s| s.name == *server_name) {
                    synced.push(format!("{}.{}", platform_name, server_name));
                }
            }
        }

        Ok(synced)
    }
}

impl Default for McpConfigRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn extract_servers_from_object(servers_object: &serde_json::Value) -> Vec<McpServerEntry> {
    let mut servers = Vec::new();

    if let Some(obj) = servers_object.as_object() {
        for (name, config) in obj {
            let enabled = config
                .get("disabled")
                .and_then(|v| v.as_bool())
                .map(|d| !d)
                .or_else(|| config.get("enabled").and_then(|v| v.as_bool()))
                .unwrap_or(true);

            servers.push(McpServerEntry {
                name: name.clone(),
                config: config.clone(),
                enabled,
            });
        }
    }

    servers
}

/// Extract named servers from a TOML array-of-tables (`[[section]]` format).
/// Each entry must have a `name` field; that field is used as the server key.
fn extract_servers_from_array(array: &serde_json::Value) -> Vec<McpServerEntry> {
    let mut servers = Vec::new();

    if let Some(arr) = array.as_array() {
        for item in arr {
            let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let mut config = item.clone();
            if let Some(obj) = config.as_object_mut() {
                obj.remove("name");
            }
            let enabled = config
                .get("disabled")
                .and_then(|v| v.as_bool())
                .map(|d| !d)
                .or_else(|| config.get("enabled").and_then(|v| v.as_bool()))
                .unwrap_or(true);
            servers.push(McpServerEntry {
                name: name.to_string(),
                config,
                enabled,
            });
        }
    }

    servers
}

/// Get the `mcp_servers_key` path for a platform from the registry.
#[allow(dead_code)]
pub fn get_mcp_servers_key_for_platform(platform: &crate::setup_module::Platform) -> &[String] {
    &platform.mcp_servers_key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> McpConfigRegistry {
        let mut reg = McpConfigRegistry::new();
        reg.platforms.push(PlatformMcpConfig {
            platform: "kiro".to_string(),
            config_path: "/home/user/.kiro/settings.json".to_string(),
            servers: vec![
                McpServerEntry {
                    name: "canopy".to_string(),
                    config: serde_json::json!({"url": "http://localhost:7755/mcp"}),
                    enabled: true,
                },
                McpServerEntry {
                    name: "github".to_string(),
                    config: serde_json::json!({"token": "abc"}),
                    enabled: true,
                },
            ],
        });
        reg.platforms.push(PlatformMcpConfig {
            platform: "opencode".to_string(),
            config_path: "/home/user/.opencode/config.json".to_string(),
            servers: vec![McpServerEntry {
                name: "canopy".to_string(),
                config: serde_json::json!({"url": "http://localhost:7755/mcp"}),
                enabled: false,
            }],
        });
        reg
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = McpConfigRegistry::new();
        assert_eq!(reg.version, 1);
        assert!(reg.platforms.is_empty());
    }

    #[test]
    fn default_registry_is_empty() {
        let reg = McpConfigRegistry::default();
        assert_eq!(reg.version, 1);
        assert!(reg.platforms.is_empty());
    }

    #[test]
    fn unique_server_names_deduped_and_sorted() {
        let reg = make_registry();
        let names = reg.unique_server_names();
        assert_eq!(names, vec!["canopy", "github"]);
    }

    #[test]
    fn unique_server_names_empty_registry() {
        let reg = McpConfigRegistry::new();
        assert!(reg.unique_server_names().is_empty());
    }

    #[test]
    fn server_diff_finds_unique_servers() {
        let reg = make_registry();
        let diff = reg.server_diff("kiro", "opencode");
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].name, "github");
    }

    #[test]
    fn server_diff_no_difference() {
        let reg = make_registry();
        let diff = reg.server_diff("opencode", "kiro");
        assert!(diff.is_empty());
    }

    #[test]
    fn server_diff_missing_platform() {
        let reg = make_registry();
        let diff = reg.server_diff("nonexistent", "kiro");
        assert!(diff.is_empty());
    }

    #[test]
    fn extract_servers_from_object_basic() {
        let obj = serde_json::json!({
            "server1": {"url": "http://localhost"},
            "server2": {"disabled": true}
        });
        let servers = extract_servers_from_object(&obj);
        assert_eq!(servers.len(), 2);
        assert!(servers.iter().any(|s| s.name == "server1" && s.enabled));
        assert!(servers.iter().any(|s| s.name == "server2" && !s.enabled));
    }

    #[test]
    fn extract_servers_from_object_empty() {
        let obj = serde_json::json!({});
        let servers = extract_servers_from_object(&obj);
        assert!(servers.is_empty());
    }

    #[test]
    fn extract_servers_from_object_not_object() {
        let val = serde_json::json!("not an object");
        let servers = extract_servers_from_object(&val);
        assert!(servers.is_empty());
    }

    #[test]
    fn extract_servers_from_array_basic() {
        let arr = serde_json::json!([
            {"name": "server1", "url": "http://localhost"},
            {"name": "server2", "disabled": true}
        ]);
        let servers = extract_servers_from_array(&arr);
        assert_eq!(servers.len(), 2);
        assert!(servers.iter().any(|s| s.name == "server1" && s.enabled));
        assert!(servers.iter().any(|s| s.name == "server2" && !s.enabled));
    }

    #[test]
    fn extract_servers_from_array_empty() {
        let arr = serde_json::json!([]);
        let servers = extract_servers_from_array(&arr);
        assert!(servers.is_empty());
    }

    #[test]
    fn extract_servers_from_array_not_array() {
        let val = serde_json::json!("not an array");
        let servers = extract_servers_from_array(&val);
        assert!(servers.is_empty());
    }

    #[test]
    fn extract_servers_from_array_skips_entries_without_name() {
        let arr = serde_json::json!([
            {"url": "http://localhost"},
            {"name": "valid"}
        ]);
        let servers = extract_servers_from_array(&arr);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "valid");
    }

    #[test]
    fn extract_servers_from_array_removes_name_from_config() {
        let arr = serde_json::json!([
            {"name": "server1", "url": "http://localhost"}
        ]);
        let servers = extract_servers_from_array(&arr);
        assert!(!servers[0].config.as_object().unwrap().contains_key("name"));
    }

    #[test]
    fn extract_servers_enabled_field() {
        let obj = serde_json::json!({
            "a": {"enabled": true},
            "b": {"enabled": false},
            "c": {},
            "d": {"disabled": true}
        });
        let servers = extract_servers_from_object(&obj);
        let find = |name: &str| servers.iter().find(|s| s.name == name).unwrap().enabled;
        assert!(find("a"));
        assert!(!find("b"));
        assert!(find("c"));
        assert!(!find("d"));
    }
}
