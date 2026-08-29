use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone)]
pub struct RegistryRaw {
    pub platforms: Vec<Platform>,
    pub canonical_servers: CanonicalServers,
}

/// Canonical MCP server definitions from `servers.toml`.
#[derive(Deserialize, Clone, Default)]
pub struct CanonicalServers {
    #[serde(default)]
    pub servers: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Platform {
    pub name: String,
    pub config_path: String,
    #[serde(default)]
    pub config_format: Option<String>,
    /// When true, TOML uses `[[section]]` array-of-tables with `name = "key"`
    /// instead of the default `[section.key]` table format.
    #[serde(default)]
    pub toml_array_format: bool,
    /// How `command` + `args` are represented:
    /// - `"separate"` (default): `"command": "x", "args": [...]`
    /// - `"merged"`: `"command": ["x", ...args]` (single array)
    #[serde(default = "default_command_format")]
    pub command_format: String,
    #[serde(alias = "servers_key")]
    pub mcp_servers_key: Vec<String>,
    #[serde(default)]
    pub deprecated_keys: Vec<String>,
    /// Keys that this platform's MCP schema does not support.
    #[serde(default)]
    pub unsupported_keys: Vec<String>,
    /// Translation map from Canopy's standard field names to this platform's names.
    /// e.g. `{"env": "environment"}`.
    #[serde(default)]
    pub fields_mapping: std::collections::HashMap<String, String>,
    /// Fields that are required by this platform, with their allowed values.
    /// e.g. `{"type": ["stdio", "http"]}`. Order is irrelevant: the value is
    /// chosen by matching the server's transport (url vs command) against
    /// known type names — see `platform_adapter::resolve_required_field_value`.
    #[serde(default)]
    pub required_fields: std::collections::HashMap<String, Vec<String>>,
    /// Per-server extra fields merged into the adapted config.
    /// e.g. `server_extras.canopy = { tools = ["*"] }`.
    #[serde(default)]
    pub server_extras: std::collections::HashMap<String, serde_json::Value>,
    /// Path to the platform's skills directory (relative to home).
    /// e.g. `".kiro/skills"`.
    #[serde(default)]
    pub skills_dir: Option<String>,
    #[serde(default)]
    pub instruction_file: Option<String>,
    #[serde(default)]
    pub cli: Option<serde_json::Value>,
}

fn default_command_format() -> String {
    "separate".to_string()
}

/// Platform with parsed CLI config (for saving to .canopy/)
pub struct PlatformWithCli {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub config_path: String,
    pub cli: Option<crate::domain::cli_config::CliConfig>,
}

impl Platform {
    pub(crate) fn to_platform_with_cli(&self) -> PlatformWithCli {
        let cli = self.cli.as_ref().and_then(|v| {
            serde_json::from_value::<crate::domain::cli_config::CliConfig>(v.clone())
                .map(|mut c| {
                    c.name = self.name.clone();
                    c.instruction_file = self.instruction_file.clone();
                    c
                })
                .ok()
        });

        PlatformWithCli {
            name: self.name.clone(),
            config_path: self.config_path.clone(),
            cli,
        }
    }
}

/// Check if a platform is available by detecting its CLI binary using the
/// shared resolver (B40). Uses the same resolution path as the spawner so
/// detection can never disagree with runtime spawning.
pub fn is_platform_available(p: &Platform) -> bool {
    p.cli
        .as_ref()
        .and_then(|v| v.get("binary").and_then(|b| b.as_str()))
        .map(|binary| {
            let path_value = std::env::var("PATH").unwrap_or_default();
            crate::domain::cli_strategy::resolve_binary_in(binary, &path_value).is_ok()
        })
        .unwrap_or(false)
}

/// Resolve the actual config file path for a platform.
///
/// Handles the `.json` ↔ `.jsonc` ambiguity (e.g. opencode supports both).
/// Returns the existing file if found, falling back to an alternate extension,
/// and finally the registry default.
pub(crate) fn resolve_config_path(home: &Path, config_path: &str) -> std::path::PathBuf {
    let primary = home.join(config_path);
    if primary.exists() {
        return primary;
    }

    // Try alternate JSON extension
    let ext = primary.extension().and_then(|e| e.to_str()).unwrap_or("");
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

/// Load the saved filesystem root path for the MCP filesystem server.
/// Returns home dir as default if not yet configured.
pub(crate) fn load_mcp_fs_root(home: &Path) -> String {
    let canopy_dir = home.join(".canopy");
    let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
    config.mcp_filesystem_root
}

/// Persist the chosen filesystem root path for reuse across setups and updates.
pub(crate) fn save_mcp_fs_root(home: &Path, root: &str) {
    let canopy_dir = home.join(".canopy");
    let mut config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
    config.mcp_filesystem_root = root.to_string();
    let _ = config.save(&canopy_dir);
}
pub(crate) fn is_binary_available(binary: &str) -> bool {
    let path_value = std::env::var("PATH").unwrap_or_default();
    crate::domain::cli_strategy::resolve_binary_in(binary, &path_value).is_ok()
}

#[allow(dead_code)]
pub fn is_configured() -> bool {
    dirs::home_dir()
        .map(|h| {
            let config = crate::domain::canopy_config::CanopyConfig::load(&h.join(".canopy"));
            config.is_configured()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_command_format_returns_separate() {
        assert_eq!(default_command_format(), "separate");
    }

    #[test]
    fn resolve_config_path_returns_primary_when_exists() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("config.json");
        std::fs::write(&primary, "{}").unwrap();
        let result = resolve_config_path(dir.path(), "config.json");
        assert_eq!(result, primary);
    }

    #[test]
    fn resolve_config_path_falls_back_to_jsonc_when_jsonc_exists() {
        let dir = tempfile::tempdir().unwrap();
        let alt = dir.path().join("config.jsonc");
        std::fs::write(&alt, "{}").unwrap();
        let result = resolve_config_path(dir.path(), "config.json");
        assert_eq!(result, alt);
    }

    #[test]
    fn resolve_config_path_falls_back_to_json_when_jsonc_requested() {
        let dir = tempfile::tempdir().unwrap();
        let alt = dir.path().join("config.json");
        std::fs::write(&alt, "{}").unwrap();
        let result = resolve_config_path(dir.path(), "config.jsonc");
        assert_eq!(result, alt);
    }

    #[test]
    fn resolve_config_path_returns_primary_when_no_alternate_exists() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("config.json");
        let result = resolve_config_path(dir.path(), "config.json");
        assert_eq!(result, primary);
        assert!(!result.exists());
    }

    #[test]
    fn resolve_config_path_non_json_extension_returns_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("config.toml");
        let result = resolve_config_path(dir.path(), "config.toml");
        assert_eq!(result, primary);
    }

    #[test]
    fn resolve_config_path_no_extension_returns_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("config");
        let result = resolve_config_path(dir.path(), "config");
        assert_eq!(result, primary);
    }

    #[test]
    fn platform_to_platform_with_cli_parses_valid_json() {
        let platform = Platform {
            name: "test-platform".to_string(),
            config_path: ".config/test/config.json".to_string(),
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
            instruction_file: Some(".instructions/test.md".to_string()),
            cli: Some(serde_json::json!({
                "binary": "test-cli",
                "install": {"type": "none"}
            })),
        };

        let result = platform.to_platform_with_cli();
        assert_eq!(result.name, "test-platform");
        assert_eq!(result.config_path, ".config/test/config.json");
        assert!(result.cli.is_some());
        let cli = result.cli.unwrap();
        assert_eq!(cli.name, "test-platform");
        assert_eq!(
            cli.instruction_file,
            Some(".instructions/test.md".to_string())
        );
    }

    #[test]
    fn platform_to_platform_with_cli_no_cli_returns_none() {
        let platform = Platform {
            name: "no-cli".to_string(),
            config_path: ".config/no-cli/config.json".to_string(),
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
            cli: None,
        };

        let result = platform.to_platform_with_cli();
        assert_eq!(result.name, "no-cli");
        assert!(result.cli.is_none());
    }

    #[test]
    fn platform_to_platform_with_cli_invalid_json_returns_none() {
        let platform = Platform {
            name: "bad-cli".to_string(),
            config_path: ".config/bad/config.json".to_string(),
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
            cli: Some(serde_json::json!("not an object")),
        };

        let result = platform.to_platform_with_cli();
        assert_eq!(result.name, "bad-cli");
        assert!(result.cli.is_none());
    }

    #[test]
    fn is_platform_available_true_for_ls() {
        let platform = Platform {
            name: "test".to_string(),
            config_path: "test.json".to_string(),
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
        assert!(is_platform_available(&platform));
    }

    #[test]
    fn is_platform_available_false_for_nonexistent_binary() {
        let platform = Platform {
            name: "test".to_string(),
            config_path: "test.json".to_string(),
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
            cli: Some(serde_json::json!({"binary": "definitely_not_a_real_binary_xyz123"})),
        };
        assert!(!is_platform_available(&platform));
    }

    #[test]
    fn is_platform_available_false_when_no_cli() {
        let platform = Platform {
            name: "test".to_string(),
            config_path: "test.json".to_string(),
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
            cli: None,
        };
        assert!(!is_platform_available(&platform));
    }

    #[test]
    fn is_binary_available_true_for_ls() {
        assert!(is_binary_available("ls"));
    }

    #[test]
    fn is_binary_available_false_for_nonexistent() {
        assert!(!is_binary_available("definitely_not_a_real_binary_xyz123"));
    }

    #[test]
    fn canonical_servers_default_is_empty() {
        let servers = CanonicalServers::default();
        assert!(servers.servers.is_empty());
    }

    #[test]
    fn canonical_servers_deserializes() {
        let json = r#"{"servers": {"my-server": {"command": "echo"}}}"#;
        let servers: CanonicalServers = serde_json::from_str(json).unwrap();
        assert_eq!(servers.servers.len(), 1);
        assert!(servers.servers.contains_key("my-server"));
    }

    #[test]
    fn platform_deserializes_with_defaults() {
        let json = r#"{
            "name": "test",
            "config_path": ".config/test/config.json",
            "mcp_servers_key": ["mcpServers"]
        }"#;
        let platform: Platform = serde_json::from_str(json).unwrap();
        assert_eq!(platform.name, "test");
        assert!(!platform.toml_array_format);
        assert_eq!(platform.command_format, "separate");
        assert!(platform.deprecated_keys.is_empty());
        assert!(platform.unsupported_keys.is_empty());
        assert!(platform.fields_mapping.is_empty());
        assert!(platform.required_fields.is_empty());
        assert!(platform.server_extras.is_empty());
        assert!(platform.skills_dir.is_none());
        assert!(platform.instruction_file.is_none());
        assert!(platform.cli.is_none());
    }

    #[test]
    fn platform_deserializes_with_all_fields() {
        let json = r#"{
            "name": "full",
            "config_path": ".config/full/config.toml",
            "config_format": "toml",
            "toml_array_format": true,
            "command_format": "merged",
            "mcp_servers_key": ["mcpServers", "servers"],
            "deprecated_keys": ["old_key"],
            "unsupported_keys": ["unsupported"],
            "fields_mapping": {"env": "environment"},
            "required_fields": {"type": ["stdio", "http"]},
            "server_extras": {"canopy": {"tools": ["*"]}},
            "skills_dir": ".full/skills",
            "instruction_file": ".full/instructions.md",
            "cli": {"binary": "full-cli"}
        }"#;
        let platform: Platform = serde_json::from_str(json).unwrap();
        assert_eq!(platform.name, "full");
        assert_eq!(platform.config_format, Some("toml".to_string()));
        assert!(platform.toml_array_format);
        assert_eq!(platform.command_format, "merged");
        assert_eq!(platform.mcp_servers_key, vec!["mcpServers", "servers"]);
        assert_eq!(platform.deprecated_keys, vec!["old_key"]);
        assert_eq!(platform.unsupported_keys, vec!["unsupported"]);
        assert_eq!(
            platform.fields_mapping.get("env"),
            Some(&"environment".to_string())
        );
        assert_eq!(
            platform.required_fields.get("type"),
            Some(&vec!["stdio".to_string(), "http".to_string()])
        );
        assert!(platform.server_extras.contains_key("canopy"));
        assert_eq!(platform.skills_dir, Some(".full/skills".to_string()));
        assert_eq!(
            platform.instruction_file,
            Some(".full/instructions.md".to_string())
        );
        assert!(platform.cli.is_some());
    }

    #[test]
    fn registry_raw_stores_platforms() {
        let platform = Platform {
            name: "p1".to_string(),
            config_path: "c1".to_string(),
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
            cli: None,
        };
        let registry = RegistryRaw {
            platforms: vec![platform],
            canonical_servers: CanonicalServers::default(),
        };
        assert_eq!(registry.platforms.len(), 1);
        assert_eq!(registry.platforms[0].name, "p1");
    }
}
