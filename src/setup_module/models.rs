use serde::Deserialize;
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

#[derive(Deserialize, Clone)]
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

/// Check if a platform is available by detecting its CLI binary in PATH.
pub fn is_platform_available(p: &Platform) -> bool {
    p.cli
        .as_ref()
        .and_then(|v| v.get("binary").and_then(|b| b.as_str()))
        .map(|binary| which::which(binary).is_ok())
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
    which::which(binary).is_ok()
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
