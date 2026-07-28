use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A skill definition from the registry.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Unique skill identifier
    pub id: String,
    /// Display name
    pub name: String,
    /// Description of what the skill does
    pub description: String,
    /// Version string
    pub version: String,
    /// Author or source
    pub author: String,
    /// Tags for categorization
    #[serde(default)]
    pub tags: Vec<String>,
    /// Installation instructions per platform
    pub install_paths: Vec<SkillInstallPath>,
}

/// Platform-specific installation path for a skill.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstallPath {
    pub platform: String,
    pub target_path: String,
    pub content: String,
}

/// Registry of available skills.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsRegistry {
    pub version: u32,
    pub skills: Vec<Skill>,
}

/// Installed skill record.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkill {
    pub id: String,
    pub platforms: Vec<String>,
    pub installed_at: chrono::DateTime<chrono::Utc>,
}

impl SkillsRegistry {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            version: 1,
            skills: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn fetch_from_registry() -> Result<Self> {
        Ok(Self::new())
    }

    /// Install a skill to selected platforms.
    #[allow(dead_code)]
    pub fn install_skill(&self, skill: &Skill, target_platforms: &[&str]) -> Result<Vec<String>> {
        let home = dirs::home_dir().context("No home directory")?;
        let mut installed = Vec::new();

        for install_path in &skill.install_paths {
            if !target_platforms.contains(&install_path.platform.as_str()) {
                continue;
            }

            let target = home.join(&install_path.target_path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }

            std::fs::write(&target, &install_path.content)?;
            installed.push(format!("{}:{}", install_path.platform, skill.id));
        }

        Ok(installed)
    }

    /// List installed skills from .canopy/skills.json.
    #[allow(dead_code)]
    pub fn list_installed() -> Result<Vec<InstalledSkill>> {
        let home = dirs::home_dir().context("No home directory")?;
        let skills_file = home.join(".canopy/skills.json");

        if !skills_file.exists() {
            return Ok(Vec::new());
        }

        let content = std::fs::read_to_string(&skills_file)?;
        let skills: Vec<InstalledSkill> = serde_json::from_str(&content)?;
        Ok(skills)
    }

    /// Save installed skills record to .canopy/skills.json.
    #[allow(dead_code)]
    pub fn save_installed(skills: &[InstalledSkill]) -> Result<()> {
        let home = dirs::home_dir().context("No home directory")?;
        let canopy_dir = home.join(".canopy");
        std::fs::create_dir_all(&canopy_dir)?;

        let content = serde_json::to_string_pretty(skills)?;
        std::fs::write(canopy_dir.join("skills.json"), content)?;
        Ok(())
    }
}

impl Default for SkillsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
pub fn extract_skills_from_platform(_platform: &str, _skills_dir: &Path) -> Result<Vec<String>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_empty_registry() {
        let registry = SkillsRegistry::new();
        assert_eq!(registry.version, 1);
        assert!(registry.skills.is_empty());
    }

    #[test]
    fn fetch_from_registry_returns_ok() {
        let result = SkillsRegistry::fetch_from_registry();
        assert!(result.is_ok());
        let registry = result.unwrap();
        assert_eq!(registry.version, 1);
        assert!(registry.skills.is_empty());
    }

    #[test]
    fn default_creates_empty_registry() {
        let registry = SkillsRegistry::default();
        assert_eq!(registry.version, 1);
        assert!(registry.skills.is_empty());
    }

    #[test]
    fn extract_skills_from_platform_returns_empty_vec() {
        let result = extract_skills_from_platform("test", Path::new("/tmp"));
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn skill_struct_can_be_created() {
        let skill = Skill {
            id: "test-skill".to_string(),
            name: "Test Skill".to_string(),
            description: "A test skill".to_string(),
            version: "1.0.0".to_string(),
            author: "Test Author".to_string(),
            tags: vec!["test".to_string()],
            install_paths: vec![],
        };
        assert_eq!(skill.id, "test-skill");
        assert_eq!(skill.name, "Test Skill");
    }

    #[test]
    fn skill_install_path_struct_can_be_created() {
        let path = SkillInstallPath {
            platform: "linux".to_string(),
            target_path: "/usr/local/bin/test".to_string(),
            content: "#!/bin/bash\necho test".to_string(),
        };
        assert_eq!(path.platform, "linux");
        assert_eq!(path.target_path, "/usr/local/bin/test");
    }

    #[test]
    fn installed_skill_struct_can_be_created() {
        let skill = InstalledSkill {
            id: "test-skill".to_string(),
            platforms: vec!["linux".to_string()],
            installed_at: chrono::Utc::now(),
        };
        assert_eq!(skill.id, "test-skill");
        assert_eq!(skill.platforms.len(), 1);
    }
}
