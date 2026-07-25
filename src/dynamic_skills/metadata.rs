//! Per-skill metadata recorded alongside the fetched content in the store,
//! at `<store_dir>/<name>/.canopy-skill.toml`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const METADATA_FILE: &str = ".canopy-skill.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub source_url: String,
    #[serde(rename = "ref", default)]
    pub git_ref: Option<String>,
    pub commit_hash: String,
    pub last_checked: DateTime<Utc>,
}

impl SkillMetadata {
    pub fn load(skill_dir: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(skill_dir.join(METADATA_FILE)).ok()?;
        toml::from_str(&content).ok()
    }

    pub fn save(&self, skill_dir: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self).context("failed to serialize skill metadata")?;
        std::fs::write(skill_dir.join(METADATA_FILE), content)
            .context("failed to write skill metadata")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn metadata_round_trip_via_toml() {
        let meta = SkillMetadata {
            source_url: "https://github.com/UniverLab/skills".to_string(),
            git_ref: Some("main".to_string()),
            commit_hash: "abc123def456".to_string(),
            last_checked: Utc::now(),
        };
        let toml_str = toml::to_string_pretty(&meta).unwrap();
        let deserialized: SkillMetadata = toml::from_str(&toml_str).unwrap();
        assert_eq!(deserialized.source_url, meta.source_url);
        assert_eq!(deserialized.git_ref.as_deref(), Some("main"));
        assert_eq!(deserialized.commit_hash, "abc123def456");
    }

    #[test]
    fn metadata_git_ref_optional() {
        let meta = SkillMetadata {
            source_url: "https://example.com/skills".to_string(),
            git_ref: None,
            commit_hash: "deadbeef".to_string(),
            last_checked: Utc::now(),
        };
        let toml_str = toml::to_string_pretty(&meta).unwrap();
        let deserialized: SkillMetadata = toml::from_str(&toml_str).unwrap();
        assert!(deserialized.git_ref.is_none());
    }

    #[test]
    fn metadata_save_and_load() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let meta = SkillMetadata {
            source_url: "https://github.com/test/skills".to_string(),
            git_ref: Some("v1.0".to_string()),
            commit_hash: "123abc".to_string(),
            last_checked: Utc::now(),
        };
        meta.save(&skill_dir).unwrap();

        let loaded = SkillMetadata::load(&skill_dir).unwrap();
        assert_eq!(loaded.source_url, meta.source_url);
        assert_eq!(loaded.git_ref.as_deref(), Some("v1.0"));
        assert_eq!(loaded.commit_hash, "123abc");
    }

    #[test]
    fn metadata_load_returns_none_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("nonexistent");
        std::fs::create_dir_all(&skill_dir).unwrap();
        assert!(SkillMetadata::load(&skill_dir).is_none());
    }

    #[test]
    fn metadata_load_returns_none_for_invalid_toml() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join(METADATA_FILE), "not valid toml {{{").unwrap();
        assert!(SkillMetadata::load(&skill_dir).is_none());
    }

    #[test]
    fn metadata_constant_file_name() {
        assert_eq!(METADATA_FILE, ".canopy-skill.toml");
    }

    #[test]
    fn metadata_clone() {
        let meta = SkillMetadata {
            source_url: "https://example.com".to_string(),
            git_ref: None,
            commit_hash: "abc".to_string(),
            last_checked: Utc::now(),
        };
        let cloned = meta.clone();
        assert_eq!(cloned.source_url, meta.source_url);
        assert_eq!(cloned.git_ref, meta.git_ref);
        assert_eq!(cloned.commit_hash, meta.commit_hash);
    }

    #[test]
    fn metadata_debug() {
        let meta = SkillMetadata {
            source_url: "https://example.com".to_string(),
            git_ref: None,
            commit_hash: "abc".to_string(),
            last_checked: Utc::now(),
        };
        let debug = format!("{:?}", meta);
        assert!(debug.contains("SkillMetadata"));
        assert!(debug.contains("abc"));
    }
}
