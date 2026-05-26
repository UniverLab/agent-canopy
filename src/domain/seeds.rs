//! Seed Identity System — persistent, evolvable agent identities.
//!
//! Each Seed is defined by a structured TOML file stored at
//! `~/.canopy/seeds/<seed_id>/identity.toml`.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maximum size of identity.toml in bytes (4 KB).
pub const MAX_IDENTITY_SIZE: usize = 4 * 1024;

/// Global seeds directory: `~/.canopy/seeds/`.
pub fn seeds_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".canopy")
        .join("seeds")
}

/// Path to a specific seed's identity.toml.
pub fn identity_path(seed_id: &str) -> PathBuf {
    seeds_dir().join(seed_id).join("identity.toml")
}

/// Structured identity for a persistent Seed agent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SeedIdentity {
    /// Unique display name (enforced case-insensitive across all seeds).
    pub name: String,
    /// Family/category label (e.g. "Trees", "Fungi").
    pub family: String,
    /// ISO 8601 creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Behavioral directives injected into prompts.
    #[serde(default)]
    pub directives: SeedDirectives,
    /// Personality/style traits.
    #[serde(default)]
    pub traits: SeedTraits,
}

/// Behavioral directives for the seed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SeedDirectives {
    /// General coding/behavior rules.
    #[serde(default)]
    pub general: Vec<String>,
}

/// Personality and style traits.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SeedTraits {
    /// Communication tone (e.g. "Concise, Technical, Patient").
    #[serde(default)]
    pub tone: Option<String>,
    /// Focus areas (e.g. "Refactoring, Bug Hunting, Architecture").
    #[serde(default)]
    pub focus: Option<String>,
}

impl SeedIdentity {
    /// Create a new seed with the current timestamp.
    #[allow(dead_code)]
    pub fn new(name: String, family: String) -> Self {
        Self {
            name,
            family,
            created_at: Utc::now(),
            directives: SeedDirectives::default(),
            traits: SeedTraits::default(),
        }
    }

    /// Serialize to TOML string.
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string(self).map_err(|e| format!("Failed to serialize identity: {e}"))
    }

    /// Deserialize from TOML string.
    pub fn from_toml(content: &str) -> Result<Self, String> {
        toml::from_str(content).map_err(|e| format!("Failed to parse identity TOML: {e}"))
    }

    /// Validate size cap (4 KB).
    pub fn validate_size(&self) -> Result<(), String> {
        let toml_str = self.to_toml()?;
        if toml_str.len() > MAX_IDENTITY_SIZE {
            return Err(format!(
                "identity.toml exceeds {MAX_IDENTITY_SIZE} bytes ({} bytes)",
                toml_str.len()
            ));
        }
        Ok(())
    }

    /// Validate mandatory fields.
    pub fn validate_fields(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Field 'name' must not be empty.".to_string());
        }
        if self.family.trim().is_empty() {
            return Err("Field 'family' must not be empty.".to_string());
        }
        Ok(())
    }

    /// Full validation: fields + size.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_fields()?;
        self.validate_size()?;
        Ok(())
    }

    /// Apply evolution updates (from evolve_identity MCP tool).
    pub fn evolve(
        &mut self,
        new_directives: Option<Vec<String>>,
        new_traits: Option<HashMap<String, String>>,
    ) -> Result<(), String> {
        if let Some(directives) = new_directives {
            self.directives.general = directives;
        }
        if let Some(traits) = new_traits {
            if let Some(tone) = traits.get("tone") {
                self.traits.tone = if tone.is_empty() {
                    None
                } else {
                    Some(tone.clone())
                };
            }
            if let Some(focus) = traits.get("focus") {
                self.traits.focus = if focus.is_empty() {
                    None
                } else {
                    Some(focus.clone())
                };
            }
        }
        self.validate()?;
        Ok(())
    }

    /// Format directives and traits as a prompt injection block.
    pub fn prompt_injection(&self) -> String {
        let mut parts = Vec::new();

        parts.push(format!("## Seed Identity: {} ({})", self.name, self.family));

        if !self.directives.general.is_empty() {
            parts.push("### Directives".to_string());
            for d in &self.directives.general {
                parts.push(format!("- {d}"));
            }
        }

        if let Some(ref tone) = self.traits.tone {
            parts.push(format!("### Tone\n{tone}"));
        }
        if let Some(ref focus) = self.traits.focus {
            parts.push(format!("### Focus\n{focus}"));
        }

        parts.join("\n\n")
    }
}

/// Check if a name is unique among all existing seeds (case-insensitive).
#[allow(dead_code)]
pub fn is_name_unique(name: &str, exclude_seed_id: Option<&str>) -> Result<bool, String> {
    let dir = seeds_dir();
    if !dir.exists() {
        return Ok(true);
    }

    let target = name.to_lowercase();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| format!("Failed to read seeds directory {}: {e}", dir.display()))?
    {
        let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }

        let seed_id = entry.file_name().to_string_lossy().to_string();
        if let Some(exclude) = exclude_seed_id {
            if seed_id == exclude {
                continue;
            }
        }

        let identity_file = entry.path().join("identity.toml");
        if identity_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&identity_file) {
                if let Ok(identity) = SeedIdentity::from_toml(&content) {
                    if identity.name.to_lowercase() == target {
                        return Ok(false);
                    }
                }
            }
        }
    }
    Ok(true)
}

/// List all existing seed IDs.
pub fn list_seeds() -> Result<Vec<String>, String> {
    let dir = seeds_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| format!("Failed to read seeds directory {}: {e}", dir.display()))?
    {
        let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            ids.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    ids.sort();
    Ok(ids)
}

/// Load a seed identity by seed_id.
pub fn load_seed(seed_id: &str) -> Result<SeedIdentity, String> {
    let path = identity_path(seed_id);
    if !path.exists() {
        return Err(format!("Seed identity not found: {seed_id}"));
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read identity at {}: {e}", path.display()))?;
    SeedIdentity::from_toml(&content)
}

/// Save a seed identity to disk (creates directory if needed).
/// Validates fields, size cap, and name uniqueness (case-insensitive).
pub fn save_seed(seed_id: &str, identity: &SeedIdentity) -> Result<(), String> {
    identity.validate()?;
    if !is_name_unique(&identity.name, Some(seed_id))? {
        return Err(format!(
            "A seed with name '{}' already exists (case-insensitive).",
            identity.name
        ));
    }
    let dir = seeds_dir().join(seed_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create seed directory {}: {e}", dir.display()))?;
    let path = dir.join("identity.toml");
    let toml_str = identity.to_toml()?;
    std::fs::write(&path, &toml_str)
        .map_err(|e| format!("Failed to write identity to {}: {e}", path.display()))?;
    Ok(())
}

/// Remove a seed identity completely.
#[allow(dead_code)]
pub fn remove_seed(seed_id: &str) -> Result<(), String> {
    let dir = seeds_dir().join(seed_id);
    if !dir.exists() {
        return Err(format!("Seed directory not found: {seed_id}"));
    }
    std::fs::remove_dir_all(&dir)
        .map_err(|e| format!("Failed to remove seed directory {}: {e}", dir.display()))?;
    Ok(())
}

/// Resolve a seed_id by name (case-insensitive).
#[allow(dead_code)]
pub fn resolve_seed_by_name(name: &str) -> Result<Option<String>, String> {
    let target = name.to_lowercase();
    for seed_id in list_seeds()? {
        if let Ok(identity) = load_seed(&seed_id) {
            if identity.name.to_lowercase() == target {
                return Ok(Some(seed_id));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "seeds_tests.rs"]
mod tests;
