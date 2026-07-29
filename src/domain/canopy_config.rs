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

    /// Global cap (F1) on how many ensemble members run concurrently across
    /// every loop run, shared by the whole daemon so one ensemble can't
    /// starve another's. An 8-member ensemble queues past this rather than
    /// fork-bombing the host.
    #[serde(default = "default_ensemble_concurrency_cap")]
    pub ensemble_concurrency_cap: usize,

    /// `[clean]` settings for the `canopy clean` CLI command.
    #[serde(default)]
    pub clean: CleanConfig,

    /// `[skills]` settings for the dynamic skill store (`~/.canopy/skills/`).
    #[serde(default)]
    pub skills: SkillsConfig,

    /// TUI color theme: `"classic"` (bordered) or `"modern"` (borderless).
    /// A plain `String` (not an enum) so a config written by a newer binary
    /// with a theme this binary doesn't know about still deserializes fine —
    /// unknown values are resolved to classic at startup, not rejected here.
    #[serde(default = "default_theme")]
    pub theme: String,

    /// Per-file indexing size cap for personal RAG, in MB. A file larger
    /// than this is skipped rather than indexed (see `canopy rag report` /
    /// `canopy doctor`). Raised from a hardcoded 5 MB to a configurable
    /// 10 MB default: on a real corpus the old constant silently skipped
    /// over a quarter of files, many of them PDFs that routinely exceed 5 MB.
    ///
    /// Raising this does not change the chunking strategy — chunks stay
    /// capped at `MAX_CHUNK_TOKENS` regardless of file size — but it does
    /// raise the worst-case chunk count (and therefore embedding calls) for
    /// a single large file roughly in proportion to the cap: a file at this
    /// 10 MB default can produce about 2x the chunks of one at the old
    /// 5 MB cap, and up to `RAG_MAX_FILE_MB_CEILING`/10 = 10x if configured
    /// all the way to the ceiling. That's an expected, bounded tradeoff of
    /// choosing to index bigger files, not a regression in chunking itself.
    #[serde(default = "default_rag_max_file_mb")]
    pub rag_max_file_mb: u32,
}

/// Highest per-file indexing cap a user may configure, in MB. Text/PDF
/// extraction loads the whole file into memory, so an unbounded (or merely
/// very large) cap risks exhausting memory on a single oversized file —
/// this is the ceiling `validate_rag_max_file_mb` enforces.
pub const RAG_MAX_FILE_MB_CEILING: u32 = 100;

fn default_rag_max_file_mb() -> u32 {
    10
}

/// Validates a configured (or user-entered) per-file indexing size cap.
/// Shared by the setup wizard's input validator and doctor's config-health
/// check, so "what counts as a valid limit" has one definition.
pub fn validate_rag_max_file_mb(mb: u32) -> Result<(), String> {
    if mb == 0 {
        return Err("Indexing size limit must be at least 1 MB".to_string());
    }
    if mb > RAG_MAX_FILE_MB_CEILING {
        return Err(format!(
            "Indexing size limit of {mb} MB exceeds the {RAG_MAX_FILE_MB_CEILING} MB ceiling"
        ));
    }
    Ok(())
}

/// One configured git source for the dynamic skill store. A skill is a
/// named top-level directory (containing `SKILL.md`) inside the source repo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSourceConfig {
    /// Git URL (https, ssh, or local path) of the skills registry repo.
    pub url: String,
    /// Branch or tag to track. Defaults to the repo's default branch.
    #[serde(rename = "ref", default)]
    pub git_ref: Option<String>,
}

/// Settings for the dynamic skill store, read from the `[skills]` table in
/// `config.toml`.
///
/// Sources are checked in list order for a skill; when two sources publish a
/// skill with the same name, the *later* source in this list wins — both for
/// `skill_list`'s merged catalog and for which source a not-yet-installed
/// skill is fetched from by `skill_get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default = "default_skill_sources")]
    pub sources: Vec<SkillSourceConfig>,
    /// Minutes a fetched skill is served from the local store before its
    /// commit hash is re-checked against the source.
    #[serde(default = "default_skill_ttl_minutes")]
    pub ttl_minutes: u64,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            sources: default_skill_sources(),
            ttl_minutes: default_skill_ttl_minutes(),
        }
    }
}

fn default_skill_sources() -> Vec<SkillSourceConfig> {
    vec![SkillSourceConfig {
        url: "https://github.com/UniverLab/skills".to_string(),
        git_ref: None,
    }]
}

fn default_skill_ttl_minutes() -> u64 {
    15
}

/// Settings for `canopy clean` (soft cleanup). Read from the `[clean]` table
/// in `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanConfig {
    /// Days of retention before orphaned/error/completed
    /// `interactive_sessions` rows and orphaned log/terminal/RAG-residue
    /// artifacts become eligible for removal. Overridable per-run with
    /// `canopy clean --older-than <days>`.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
}

impl Default for CleanConfig {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
        }
    }
}

fn default_retention_days() -> u64 {
    7
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

fn default_ensemble_concurrency_cap() -> usize {
    4
}

fn default_theme() -> String {
    "classic".to_string()
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

    /// The effective per-file indexing size cap, in bytes, read fresh from
    /// this config (callers reload `CanopyConfig` per use, so a config
    /// change is picked up on the next indexing pass without restarting the
    /// daemon). Falls back to the default rather than an out-of-range
    /// configured value — a hand-edited config.toml can set
    /// `rag_max_file_mb` to anything, but this is the one place that value
    /// actually becomes a byte limit, so it's also the one place the safety
    /// ceiling is non-negotiable. `canopy doctor` separately surfaces an
    /// invalid value as a config-health issue rather than silently ignoring it.
    pub fn rag_max_file_bytes(&self) -> u64 {
        let mb = if validate_rag_max_file_mb(self.rag_max_file_mb).is_ok() {
            self.rag_max_file_mb
        } else {
            default_rag_max_file_mb()
        };
        mb as u64 * 1024 * 1024
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
            ensemble_concurrency_cap: default_ensemble_concurrency_cap(),
            clean: CleanConfig::default(),
            skills: SkillsConfig::default(),
            theme: default_theme(),
            rag_max_file_mb: default_rag_max_file_mb(),
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
        assert_eq!(config.theme, "classic");
        assert_eq!(config.rag_max_file_mb, 10);
    }

    #[test]
    fn test_config_without_theme_field_uses_classic_default() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        // Simulates a config written before the `theme` field existed.
        let toml = r#"embeddings_model = "intfloat/multilingual-e5-base""#;
        std::fs::write(canopy_dir.join("config.toml"), toml).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.theme, "classic");
    }

    #[test]
    fn rag_max_file_mb_defaults_to_10_when_absent_from_config_toml() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        // Simulates a config written before this field existed.
        let toml = r#"embeddings_model = "intfloat/multilingual-e5-base""#;
        std::fs::write(canopy_dir.join("config.toml"), toml).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.rag_max_file_mb, 10);
        assert_eq!(loaded.rag_max_file_bytes(), 10 * 1024 * 1024);
    }

    #[test]
    fn rag_max_file_mb_round_trips_via_config_toml() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();

        let config = CanopyConfig {
            rag_max_file_mb: 25,
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.rag_max_file_mb, 25);
        assert_eq!(loaded.rag_max_file_bytes(), 25 * 1024 * 1024);
    }

    #[test]
    fn validate_rag_max_file_mb_rejects_zero() {
        assert!(validate_rag_max_file_mb(0).is_err());
    }

    #[test]
    fn validate_rag_max_file_mb_rejects_above_ceiling() {
        let err = validate_rag_max_file_mb(RAG_MAX_FILE_MB_CEILING + 1)
            .expect_err("value above the ceiling must be rejected");
        assert!(err.contains(&RAG_MAX_FILE_MB_CEILING.to_string()));
    }

    #[test]
    fn validate_rag_max_file_mb_accepts_the_ceiling_itself() {
        assert!(validate_rag_max_file_mb(RAG_MAX_FILE_MB_CEILING).is_ok());
    }

    #[test]
    fn validate_rag_max_file_mb_accepts_ordinary_values() {
        assert!(validate_rag_max_file_mb(1).is_ok());
        assert!(validate_rag_max_file_mb(10).is_ok());
        assert!(validate_rag_max_file_mb(50).is_ok());
    }

    /// `rag_max_file_bytes` is the one place an invalid persisted value
    /// actually becomes a byte limit — it must fall back to the default
    /// rather than honor an out-of-range hand-edited config.toml, since a
    /// single oversized cap risks exhausting memory during extraction.
    #[test]
    fn rag_max_file_bytes_falls_back_to_default_when_configured_value_is_invalid() {
        let mut config = CanopyConfig {
            rag_max_file_mb: RAG_MAX_FILE_MB_CEILING + 50,
            ..Default::default()
        };
        assert_eq!(config.rag_max_file_bytes(), 10 * 1024 * 1024);

        config.rag_max_file_mb = 0;
        assert_eq!(config.rag_max_file_bytes(), 10 * 1024 * 1024);
    }

    #[test]
    fn test_theme_round_trips_via_config_toml() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");

        let config = CanopyConfig {
            theme: "modern".to_string(),
            ..CanopyConfig::default()
        };
        config.save(&canopy_dir).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.theme, "modern");
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
    fn test_config_without_ensemble_cap_field_uses_default_of_four() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        // Simulates a config written before `ensemble_concurrency_cap` existed.
        let toml = r#"embeddings_model = "intfloat/multilingual-e5-base""#;
        std::fs::write(canopy_dir.join("config.toml"), toml).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.ensemble_concurrency_cap, 4);
    }

    #[test]
    fn test_config_without_clean_section_uses_default_retention_of_seven_days() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        // Simulates a config written before the `[clean]` table existed.
        let toml = r#"embeddings_model = "intfloat/multilingual-e5-base""#;
        std::fs::write(canopy_dir.join("config.toml"), toml).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.clean.retention_days, 7);
    }

    #[test]
    fn test_clean_retention_days_round_trips_via_config_toml() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let toml = "[clean]\nretention_days = 3\n";
        std::fs::write(canopy_dir.join("config.toml"), toml).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.clean.retention_days, 3);
    }

    #[test]
    fn test_default_skills_config_has_one_source_and_15min_ttl() {
        let config = CanopyConfig::default();
        assert_eq!(config.skills.sources.len(), 1);
        assert_eq!(
            config.skills.sources[0].url,
            "https://github.com/UniverLab/skills"
        );
        assert_eq!(config.skills.sources[0].git_ref, None);
        assert_eq!(config.skills.ttl_minutes, 15);
    }

    #[test]
    fn test_skills_config_round_trips_via_config_toml() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        let config = CanopyConfig {
            skills: SkillsConfig {
                sources: vec![
                    SkillSourceConfig {
                        url: "https://example.com/skills-a".to_string(),
                        git_ref: None,
                    },
                    SkillSourceConfig {
                        url: "https://example.com/skills-b".to_string(),
                        git_ref: Some("v2".to_string()),
                    },
                ],
                ttl_minutes: 30,
            },
            ..CanopyConfig::default()
        };
        config.save(&canopy_dir).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.skills.sources.len(), 2);
        assert_eq!(loaded.skills.sources[1].git_ref.as_deref(), Some("v2"));
        assert_eq!(loaded.skills.ttl_minutes, 30);
    }

    #[test]
    fn test_config_without_skills_section_uses_default_source() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let toml = r#"embeddings_model = "intfloat/multilingual-e5-base""#;
        std::fs::write(canopy_dir.join("config.toml"), toml).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.skills.sources.len(), 1);
        assert_eq!(loaded.skills.ttl_minutes, 15);
    }

    #[test]
    fn test_ensemble_concurrency_cap_round_trips() {
        let dir = TempDir::new().unwrap();
        let canopy_dir = dir.path().join(".canopy");

        let config = CanopyConfig {
            ensemble_concurrency_cap: 8,
            ..CanopyConfig::default()
        };
        config.save(&canopy_dir).unwrap();

        let loaded = CanopyConfig::load(&canopy_dir);
        assert_eq!(loaded.ensemble_concurrency_cap, 8);
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
