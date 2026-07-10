//! Unified canopy configuration (`~/.canopy/config.toml`).

use serde::{Deserialize, Serialize};
use std::path::Path;

use super::cli_config::CliConfig;

/// Top-level canopy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanopyConfig {
    /// RFC 3339 timestamp of when setup was last completed.
    /// If `None`, setup has not been run yet.
    #[serde(default)]
    pub configured_at: Option<String>,

    /// Root directory for the MCP filesystem server.
    #[serde(default = "default_mcp_root")]
    pub mcp_filesystem_root: String,

    /// Available CLIs detected during setup.
    #[serde(default)]
    pub clis: Vec<CliConfig>,

    /// Temperature unit used by sysinfo widgets.
    #[serde(default)]
    pub temperature_unit: TemperatureUnit,

    /// Embeddings model identifier used by the knowledge layer.
    #[serde(default)]
    pub embeddings_model: String,

    /// Lexical-cohesion threshold for semantic chunk merging (0.0 - 1.0).
    /// Adjacent chunks with term-frequency cosine similarity at or above this
    /// value are merged. Measured on real docs: same-topic neighbors score
    /// ~0.2-0.5, unrelated ones ~0.0-0.15 — hence the 0.25 default.
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f32,

    /// Personal RAG directories — all are indexed recursively.
    /// Replaces the old `rag_personal_root` single-path field.
    #[serde(default)]
    pub rag_personal_dirs: Vec<String>,

    /// Legacy single-path field kept for backward-compat deserialization only.
    /// Migrated to `rag_personal_dirs` on first load.
    #[serde(default, skip_serializing)]
    pub rag_personal_root: String,

    /// Root path used to discover or group related projects.
    #[serde(default = "default_projects_root")]
    pub projects_root: String,

    /// Seconds of inactivity after which the (lazily-loaded) embedding model
    /// is dropped from memory. Reloaded transparently on next use.
    #[serde(default = "default_embeddings_idle_unload_secs")]
    pub embeddings_idle_unload_secs: u64,
}

/// Preferred unit for temperature display.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TemperatureUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

fn default_mcp_root() -> String {
    dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|| "/".to_string())
}

fn default_similarity_threshold() -> f32 {
    0.25
}

fn default_embeddings_idle_unload_secs() -> u64 {
    600
}

fn default_projects_root() -> String {
    if let Some(home) = dirs::home_dir() {
        let preferred = home.join("Documents").join("Projects");
        if preferred.exists() {
            return preferred.to_string_lossy().to_string();
        }
        return home.to_string_lossy().to_string();
    }
    "/".to_string()
}

impl CanopyConfig {
    /// Load config from `~/.canopy/config.toml`. Returns default if not found.
    pub fn load(canopy_dir: &Path) -> Self {
        let config_path = canopy_dir.join("config.toml");
        let mut config: CanopyConfig = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|content| toml::from_str::<CanopyConfig>(&content).ok())
            .unwrap_or_default();
        // Migrate legacy single-root field to the new multi-dir vec.
        if config.rag_personal_dirs.is_empty() && !config.rag_personal_root.is_empty() {
            config
                .rag_personal_dirs
                .push(config.rag_personal_root.clone());
        }
        config
    }

    /// Save config to `~/.canopy/config.toml`.
    pub fn save(&self, canopy_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(canopy_dir)?;
        let content = toml::to_string_pretty(self).unwrap_or_default();
        std::fs::write(canopy_dir.join("config.toml"), content)
    }

    /// Whether setup has been completed.
    pub fn is_configured(&self) -> bool {
        self.configured_at.is_some()
    }

    /// Mark setup as completed (sets `configured_at` to now).
    pub fn mark_configured(&mut self) {
        self.configured_at = Some(chrono::Utc::now().to_rfc3339());
    }

    /// Get a CLI config by name.
    pub fn get_cli(&self, name: &str) -> Option<&CliConfig> {
        self.clis.iter().find(|c| c.name == name)
    }

    /// Get all available CLI names.
    pub fn cli_names(&self) -> Vec<&str> {
        self.clis.iter().map(|c| c.name.as_str()).collect()
    }
}

impl Default for CanopyConfig {
    fn default() -> Self {
        Self {
            configured_at: None,
            mcp_filesystem_root: default_mcp_root(),
            clis: Vec::new(),
            temperature_unit: TemperatureUnit::default(),
            embeddings_model: String::new(),
            similarity_threshold: default_similarity_threshold(),
            rag_personal_dirs: Vec::new(),
            rag_personal_root: String::new(),
            projects_root: default_projects_root(),
            embeddings_idle_unload_secs: default_embeddings_idle_unload_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = CanopyConfig::default();
        assert!(!config.is_configured());
        assert!(config.clis.is_empty());
        assert_eq!(config.temperature_unit, TemperatureUnit::Celsius);
        assert_eq!(config.embeddings_model, "");
        assert_eq!(config.similarity_threshold, 0.25);
    }

    #[test]
    fn test_save_and_load() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");

        let mut config = CanopyConfig::default();
        config.mark_configured();
        config.mcp_filesystem_root = "/custom/path".to_string();
        config.temperature_unit = TemperatureUnit::Fahrenheit;
        config.embeddings_model = "custom-embed".to_string();
        config.similarity_threshold = 0.35;
        config.rag_personal_dirs = vec!["/rag/home".to_string(), "/rag/docs".to_string()];
        config.projects_root = "/projects".to_string();

        config.save(&canopy_dir).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert!(loaded.is_configured());
        assert_eq!(loaded.mcp_filesystem_root, "/custom/path");
        assert_eq!(loaded.temperature_unit, TemperatureUnit::Fahrenheit);
        assert_eq!(loaded.embeddings_model, "custom-embed");
        assert_eq!(loaded.rag_personal_dirs, vec!["/rag/home", "/rag/docs"]);
        assert_eq!(loaded.projects_root, "/projects");
    }

    #[test]
    fn test_legacy_rag_personal_root_migration() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        // Write a config with the old single-root field.
        let toml = r#"rag_personal_root = "/old/rag""#;
        std::fs::write(canopy_dir.join("config.toml"), toml).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.rag_personal_dirs, vec!["/old/rag"]);
    }

    #[test]
    fn test_config_without_idle_unload_field_uses_default() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        // Simulates a config written before `embeddings_idle_unload_secs` existed.
        let toml = r#"embeddings_model = "intfloat/multilingual-e5-base""#;
        std::fs::write(canopy_dir.join("config.toml"), toml).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.embeddings_idle_unload_secs, 600);
    }

    #[test]
    fn test_load_missing_returns_default() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");

        let config = CanopyConfig::load(&canopy_dir);
        assert!(!config.is_configured());
        assert!(config.clis.is_empty());
    }

    #[test]
    fn test_get_cli() {
        let mut config = CanopyConfig::default();
        config.clis.push(CliConfig {
            name: "opencode".to_string(),
            binary: "opencode".to_string(),
            ..Default::default()
        });

        assert!(config.get_cli("opencode").is_some());
        assert!(config.get_cli("nonexistent").is_none());
    }

    #[test]
    fn test_cli_names() {
        let mut config = CanopyConfig::default();
        config.clis.push(CliConfig {
            name: "opencode".to_string(),
            ..Default::default()
        });
        config.clis.push(CliConfig {
            name: "kiro".to_string(),
            ..Default::default()
        });

        let names = config.cli_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"opencode"));
        assert!(names.contains(&"kiro"));
    }
}
