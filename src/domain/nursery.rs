//! Nursery — temporary workspace for collaboratively defining a new seed identity.
//!
//! When the user selects "Plant New Seed" in the TUI, a temporary directory is created
//! containing:
//! - A draft `identity.toml` with empty/default fields
//! - An instruction file (e.g. `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`) containing
//!   "Gardener Instructions" that guide the agent to interview the user and define
//!   the seed's name, family, directives, and traits.

use std::path::PathBuf;

use crate::domain::seeds::{self, SeedDirectives, SeedIdentity, SeedTraits};

/// Gardener instructions that guide the agent to collaborate with the user
/// to define a new seed identity.
pub const GARDENER_INSTRUCTIONS: &str = r#"# Seed Nursery — Gardener Instructions

You are helping the user define a new persistent **Seed Identity** for Canopy.
A Seed is a named agent identity with behavioral directives and traits that persists
across sessions.

## Your Task

Work with the user to define the following, then write them to `identity.toml` in this directory:

1. **name** — A unique display name (e.g. "Liquidambar", "Quercus", "Boletus"). Use a plant, fungi, or nature-inspired name.
2. **family** — A category label (e.g. "Trees", "Fungi", "Minerals", "Weather").
3. **directives.general** — A list of behavioral rules (e.g. "Prioritize type-safety", "Explain structural changes before executing").
4. **traits.tone** — Communication style (e.g. "Concise, Technical, Patient").
5. **traits.focus** — Specialization areas (e.g. "Refactoring, Bug Hunting, Architecture").

## Process

1. Greet the user and explain you're helping define a new Seed identity.
2. Ask for a name suggestion. If they're stuck, suggest some nature-inspired names.
3. Ask for a family/category.
4. Ask what behavioral directives they want (coding style, safety rules, etc.).
5. Ask about tone and focus preferences.
6. Write the final `identity.toml` using the format below.
7. Confirm with the user that everything looks correct.

## identity.toml Format

```toml
name = "TheName"
family = "TheFamily"
created_at = "2026-01-01T00:00:00Z"

[directives]
general = [
    "Directive 1",
    "Directive 2"
]

[traits]
tone = "Concise, Technical"
focus = "Refactoring, Architecture"
```

## Important

- The `name` must be unique across all seeds (case-insensitive).
- Keep directives concise and actionable.
- When the user is satisfied, the identity will be validated and registered automatically.
"#;

/// Instruction file name per CLI platform.
/// Falls back to "AGENTS.md" for unknown platforms.
pub fn instruction_file_for_cli(cli_name: &str) -> &'static str {
    match cli_name {
        "claude" => "CLAUDE.md",
        "gemini" => "GEMINI.md",
        "copilot" => ".github/copilot-instructions.md",
        "opencode" => "AGENTS.md",
        "kiro" => "AGENTS.md",
        "codex" => "AGENTS.md",
        "mistral" => "AGENTS.md",
        _ => "AGENTS.md",
    }
}

/// Create a temporary nursery workspace for a new seed.
/// Returns the path to the temporary directory.
pub fn create_nursery(cli_name: &str) -> Result<PathBuf, String> {
    let temp_dir = std::env::temp_dir().join(format!("canopy-nursery-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create nursery directory: {e}"))?;

    // Write draft identity.toml
    let identity = SeedIdentity {
        name: String::new(),
        family: String::new(),
        created_at: chrono::Utc::now(),
        directives: SeedDirectives::default(),
        traits: SeedTraits::default(),
    };
    let identity_path = temp_dir.join("identity.toml");
    std::fs::write(&identity_path, identity.to_toml()?)
        .map_err(|e| format!("Failed to write draft identity: {e}"))?;

    // Write instruction file for the selected CLI
    let instr_filename = instruction_file_for_cli(cli_name);
    let instr_path = temp_dir.join(instr_filename);

    // For nested paths like .github/copilot-instructions.md, create parent dirs
    if let Some(parent) = instr_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create instruction parent directory: {e}"))?;
    }

    std::fs::write(&instr_path, GARDENER_INSTRUCTIONS)
        .map_err(|e| format!("Failed to write gardener instructions: {e}"))?;

    Ok(temp_dir)
}

/// Validate and finalize a nursery session.
/// Reads the identity.toml from the nursery temp_dir, validates it,
/// and moves it to the global seeds directory.
/// Returns the seed_id on success.
pub fn finalize_nursery(temp_dir: &PathBuf) -> Result<String, String> {
    let identity_path = temp_dir.join("identity.toml");
    if !identity_path.exists() {
        return Err("identity.toml not found in nursery directory".to_string());
    }

    let content = std::fs::read_to_string(&identity_path)
        .map_err(|e| format!("Failed to read identity.toml: {e}"))?;
    let identity = SeedIdentity::from_toml(&content)?;

    // Validate fields and size
    identity.validate()?;

    // Check name uniqueness
    if !seeds::is_name_unique(&identity.name, None)? {
        return Err(format!(
            "A seed with name '{}' already exists (case-insensitive).",
            identity.name
        ));
    }

    // Generate seed_id from name (lowercase, slugified)
    let seed_id = slugify(&identity.name);

    // Save to global seeds directory
    seeds::save_seed(&seed_id, &identity)?;

    // Clean up temp directory
    let _ = std::fs::remove_dir_all(temp_dir);

    Ok(seed_id)
}

/// Convert a name to a filesystem-safe slug.
pub fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_file_mapping() {
        assert_eq!(instruction_file_for_cli("claude"), "CLAUDE.md");
        assert_eq!(instruction_file_for_cli("gemini"), "GEMINI.md");
        assert_eq!(
            instruction_file_for_cli("copilot"),
            ".github/copilot-instructions.md"
        );
        assert_eq!(instruction_file_for_cli("opencode"), "AGENTS.md");
        assert_eq!(instruction_file_for_cli("kiro"), "AGENTS.md");
        assert_eq!(instruction_file_for_cli("codex"), "AGENTS.md");
        assert_eq!(instruction_file_for_cli("mistral"), "AGENTS.md");
        assert_eq!(instruction_file_for_cli("unknown"), "AGENTS.md");
    }

    #[test]
    fn slugify_converts_name() {
        assert_eq!(slugify("Liquidambar"), "liquidambar");
        assert_eq!(slugify("Red Oak"), "red-oak");
        assert_eq!(slugify("Boletus edulis"), "boletus-edulis");
        assert_eq!(slugify("Test_123"), "test_123");
        assert_eq!(slugify("My-Seed"), "my-seed");
        assert_eq!(slugify("UPPER"), "upper");
    }

    #[test]
    fn gardener_instructions_not_empty() {
        assert!(!GARDENER_INSTRUCTIONS.is_empty());
        assert!(GARDENER_INSTRUCTIONS.contains("identity.toml"));
        assert!(GARDENER_INSTRUCTIONS.contains("name"));
        assert!(GARDENER_INSTRUCTIONS.contains("family"));
        assert!(GARDENER_INSTRUCTIONS.contains("directives"));
        assert!(GARDENER_INSTRUCTIONS.contains("traits"));
    }

    #[test]
    fn create_nursery_creates_temp_directory() {
        let result = create_nursery("opencode");
        assert!(result.is_ok());
        let temp_dir = result.unwrap();
        assert!(temp_dir.exists());
        assert!(temp_dir.starts_with(std::env::temp_dir()));
        assert!(temp_dir.to_string_lossy().contains("canopy-nursery-"));

        // Clean up
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn create_nursery_writes_identity_toml() {
        let temp_dir = create_nursery("claude").unwrap();
        let identity_path = temp_dir.join("identity.toml");
        assert!(identity_path.exists());

        let content = std::fs::read_to_string(&identity_path).unwrap();
        assert!(content.contains("name = \"\""));
        assert!(content.contains("family = \"\""));
        assert!(content.contains("created_at"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn create_nursery_writes_instruction_file_for_cli() {
        // Test flat path (AGENTS.md)
        let temp_dir = create_nursery("opencode").unwrap();
        assert!(temp_dir.join("AGENTS.md").exists());
        let content = std::fs::read_to_string(temp_dir.join("AGENTS.md")).unwrap();
        assert!(content.contains("Seed Nursery"));
        let _ = std::fs::remove_dir_all(&temp_dir);

        // Test flat path (CLAUDE.md)
        let temp_dir = create_nursery("claude").unwrap();
        assert!(temp_dir.join("CLAUDE.md").exists());
        let _ = std::fs::remove_dir_all(&temp_dir);

        // Test nested path (.github/copilot-instructions.md)
        let temp_dir = create_nursery("copilot").unwrap();
        let instr_path = temp_dir.join(".github/copilot-instructions.md");
        assert!(instr_path.exists());
        let content = std::fs::read_to_string(&instr_path).unwrap();
        assert!(content.contains("Seed Nursery"));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn create_nursery_identity_is_parseable() {
        let temp_dir = create_nursery("gemini").unwrap();
        let content = std::fs::read_to_string(temp_dir.join("identity.toml")).unwrap();
        let identity = SeedIdentity::from_toml(&content).unwrap();
        assert!(identity.name.is_empty());
        assert!(identity.family.is_empty());
        assert!(identity.directives.general.is_empty());
        assert!(identity.traits.tone.is_none());
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn finalize_nursery_valid_identity() {
        let temp_dir = create_nursery("opencode").unwrap();

        // Write a valid identity
        let identity = SeedIdentity::new("TestNurseryOak".to_string(), "Trees".to_string());
        std::fs::write(temp_dir.join("identity.toml"), identity.to_toml().unwrap()).unwrap();

        let seed_id = finalize_nursery(&temp_dir).unwrap();
        assert_eq!(seed_id, "testnurseryoak");

        // Verify seed was created
        assert!(seeds::load_seed(&seed_id).is_ok());
        let loaded = seeds::load_seed(&seed_id).unwrap();
        assert_eq!(loaded.name, "TestNurseryOak");
        assert_eq!(loaded.family, "Trees");

        // Verify temp dir was cleaned up
        assert!(!temp_dir.exists());

        // Clean up the created seed
        let _ = seeds::remove_seed(&seed_id);
    }

    #[test]
    fn finalize_nursery_rejects_empty_name() {
        let temp_dir = create_nursery("opencode").unwrap();
        // Leave identity.toml with empty name (default from create_nursery)

        let result = finalize_nursery(&temp_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("name"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn finalize_nursery_rejects_missing_identity() {
        let temp_dir = create_nursery("opencode").unwrap();
        // Remove the identity.toml
        std::fs::remove_file(temp_dir.join("identity.toml")).unwrap();

        let result = finalize_nursery(&temp_dir);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn finalize_nursery_rejects_name_collision() {
        // First, create a seed with a known name
        let existing = SeedIdentity::new("UniqueNurseryName".to_string(), "Fungi".to_string());
        let existing_id = slugify("UniqueNurseryName");
        seeds::save_seed(&existing_id, &existing).unwrap();

        // Now try to finalize a nursery with the same name
        let temp_dir = create_nursery("opencode").unwrap();
        let identity = SeedIdentity::new("UniqueNurseryName".to_string(), "Trees".to_string());
        std::fs::write(temp_dir.join("identity.toml"), identity.to_toml().unwrap()).unwrap();

        let result = finalize_nursery(&temp_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));

        // Clean up
        let _ = seeds::remove_seed(&existing_id);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn finalize_nursery_slugifies_special_characters() {
        let temp_dir = create_nursery("opencode").unwrap();
        let identity = SeedIdentity::new("Red Oak 🌳".to_string(), "Trees".to_string());
        std::fs::write(temp_dir.join("identity.toml"), identity.to_toml().unwrap()).unwrap();

        let seed_id = finalize_nursery(&temp_dir).unwrap();
        assert_eq!(seed_id, "red-oak-");

        let _ = seeds::remove_seed(&seed_id);
    }
}
