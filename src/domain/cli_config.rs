//! Registry-driven CLI configuration.
//!
//! All CLI definitions come from the canopy registry (`platforms.json`).
//! During setup, available CLIs are detected and saved to `~/.canopy/cli_config.json`.
//! The executor uses this saved config to build commands dynamically --
//! no hard-coded strategies needed.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Complete CLI definition from the registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub binary: String,
    #[serde(default)]
    pub headless_mode: String,
    #[serde(default)]
    pub model_flag: Option<String>,
    #[serde(default)]
    pub supports_working_dir: bool,
    #[serde(default)]
    pub working_dir_flag: Option<String>,
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,
    /// Arguments to pass when launching in interactive (TUI) mode.
    #[serde(default)]
    pub interactive_args: Option<String>,
    /// Fallback interactive args if the primary mode fails to start (e.g. `kiro-cli --tui` → `kiro-cli chat`).
    #[serde(default)]
    pub fallback_interactive_args: Option<String>,
    /// Arguments to pass when launching in resume mode (most recent session).
    #[serde(default)]
    pub resume_args: Option<String>,
    /// Subcommand/args to run to list sessions, e.g. `"session list"`.
    /// When set, the new-agent dialog shows a canopy-side session picker.
    #[serde(default)]
    pub session_list_cmd: Option<String>,
    /// Flag to resume a specific session by ID, e.g. `"--session"`.
    /// The session ID is appended as the next argument.
    #[serde(default)]
    pub session_resume_cmd: Option<String>,
    /// RGB accent color for this CLI's agents in the TUI.
    #[serde(default)]
    pub accent_color: Option<[u8; 3]>,
    /// Flag to pass to disable approval prompts (yolo/autonomous mode).
    #[serde(default)]
    pub yolo_flag: Option<String>,
    /// Path to the custom instructions file (e.g. `.github/copilot-instructions.md`).
    #[serde(default)]
    pub instruction_file: Option<String>,
    /// When true, the composed prompt is written to a temp file and piped in
    /// via stdin instead of being passed as a command-line argument, keeping
    /// argv small and fixed-size regardless of prompt size. Only set this for
    /// CLIs that read the prompt from stdin when none is given as an argument
    /// (e.g. `claude -p`). Defaults to `false` (legacy argv behavior), since
    /// most CLIs require the prompt as a positional argument.
    #[serde(default)]
    pub prompt_via_stdin: bool,
}

/// Persisted CLI configuration for available CLIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliRegistry {
    /// Version of the config format
    pub version: u32,
    /// Available CLIs detected during setup
    pub available_clis: Vec<CliConfig>,
}

impl CliConfig {
    /// Check if this CLI is available in PATH.
    pub fn is_available(&self) -> bool {
        which::which(&self.binary).is_ok()
    }
}

impl CliRegistry {
    /// Create a new registry with the current config version.
    pub fn new() -> Self {
        Self {
            version: 2,
            available_clis: Vec::new(),
        }
    }

    /// Detect which CLIs from a list are available in PATH.
    pub fn detect_available(platforms: &[crate::setup_module::PlatformWithCli]) -> Self {
        let mut registry = Self::new();

        for platform in platforms {
            if let Some(ref cli) = platform.cli {
                if cli.is_available() {
                    registry.available_clis.push(cli.clone());
                }
            }
        }

        registry
    }

    /// Save this configuration to a file.
    #[allow(dead_code)]
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)
    }

    /// Load configuration from a file.
    #[allow(dead_code)]
    pub fn load(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Get a CLI config by name.
    pub fn get(&self, name: &str) -> Option<&CliConfig> {
        self.available_clis.iter().find(|c| c.name == name)
    }

    /// Get all available CLI names.
    #[allow(dead_code)]
    pub fn names(&self) -> Vec<&str> {
        self.available_clis
            .iter()
            .map(|c| c.name.as_str())
            .collect()
    }
}

impl Default for CliRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_cli_config() -> CliConfig {
        CliConfig {
            name: "opencode".to_string(),
            binary: "opencode".to_string(),
            headless_mode: "--headless".to_string(),
            model_flag: Some("--model".to_string()),
            supports_working_dir: true,
            working_dir_flag: Some("--dir".to_string()),
            env_vars: std::collections::HashMap::new(),
            interactive_args: None,
            fallback_interactive_args: None,
            resume_args: None,
            session_list_cmd: None,
            session_resume_cmd: None,
            accent_color: None,
            yolo_flag: None,
            instruction_file: None,
            prompt_via_stdin: false,
        }
    }

    #[test]
    fn test_cli_registry_new_sets_version() {
        let registry = CliRegistry::new();
        assert_eq!(registry.version, 2);
        assert!(registry.available_clis.is_empty());
    }

    #[test]
    fn test_cli_registry_default() {
        let registry = CliRegistry::default();
        assert_eq!(registry.version, 2);
        assert!(registry.available_clis.is_empty());
    }

    #[test]
    fn test_cli_registry_get_found() {
        let mut registry = CliRegistry::new();
        registry.available_clis.push(sample_cli_config());
        let config = registry.get("opencode");
        assert!(config.is_some());
        assert_eq!(config.unwrap().binary, "opencode");
    }

    #[test]
    fn test_cli_registry_get_not_found() {
        let registry = CliRegistry::new();
        let config = registry.get("nonexistent");
        assert!(config.is_none());
    }

    #[test]
    fn test_cli_registry_names() {
        let mut registry = CliRegistry::new();
        let mut cli1 = sample_cli_config();
        cli1.name = "opencode".to_string();
        let mut cli2 = sample_cli_config();
        cli2.name = "kiro".to_string();
        registry.available_clis.push(cli1);
        registry.available_clis.push(cli2);

        let names = registry.names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"opencode"));
        assert!(names.contains(&"kiro"));
    }

    #[test]
    fn test_cli_registry_save_and_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cli_config.json");

        let mut registry = CliRegistry::new();
        registry.available_clis.push(sample_cli_config());

        registry.save(&path).unwrap();

        let loaded = CliRegistry::load(&path).unwrap();
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.available_clis.len(), 1);
        assert_eq!(loaded.available_clis[0].name, "opencode");
    }

    #[test]
    fn test_cli_registry_load_nonexistent() {
        let path = std::path::Path::new("/nonexistent/path/config.json");
        let loaded = CliRegistry::load(path);
        assert!(loaded.is_none());
    }

    #[test]
    fn test_cli_registry_save_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir
            .path()
            .join("nested")
            .join("dir")
            .join("cli_config.json");

        let registry = CliRegistry::new();
        registry.save(&path).unwrap();

        assert!(path.exists());
    }
}
