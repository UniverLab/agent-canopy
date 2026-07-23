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
