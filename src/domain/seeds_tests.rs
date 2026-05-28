use std::collections::HashMap;

use super::*;

fn make_test_identity() -> SeedIdentity {
    let mut identity = SeedIdentity::new("TestOak".to_string(), "Trees".to_string());
    identity.directives.general = vec![
        "Prioritize type-safety.".to_string(),
        "Write tests first.".to_string(),
    ];
    identity.traits.tone = Some("Concise, Technical".to_string());
    identity.traits.focus = Some("Refactoring".to_string());
    identity
}

#[test]
fn test_new_identity_has_defaults() {
    let identity = SeedIdentity::new("Liquidambar".to_string(), "Trees".to_string());
    assert_eq!(identity.name, "Liquidambar");
    assert_eq!(identity.family, "Trees");
    assert!(identity.directives.general.is_empty());
    assert!(identity.traits.tone.is_none());
    assert!(identity.traits.focus.is_none());
}

#[test]
fn test_toml_roundtrip() {
    let identity = make_test_identity();
    let toml_str = identity.to_toml().unwrap();
    let parsed = SeedIdentity::from_toml(&toml_str).unwrap();
    assert_eq!(parsed.name, identity.name);
    assert_eq!(parsed.family, identity.family);
    assert_eq!(parsed.directives.general, identity.directives.general);
    assert_eq!(parsed.traits.tone, identity.traits.tone);
    assert_eq!(parsed.traits.focus, identity.traits.focus);
}

#[test]
fn test_validate_fields_passes() {
    let identity = make_test_identity();
    assert!(identity.validate_fields().is_ok());
}

#[test]
fn test_validate_fields_empty_name() {
    let mut identity = make_test_identity();
    identity.name = String::new();
    assert!(identity.validate_fields().is_err());
}

#[test]
fn test_validate_fields_empty_family() {
    let mut identity = make_test_identity();
    identity.family = String::new();
    assert!(identity.validate_fields().is_err());
}

#[test]
fn test_validate_size_passes_for_normal_identity() {
    let identity = make_test_identity();
    assert!(identity.validate_size().is_ok());
}

#[test]
fn test_prompt_injection_contains_directives() {
    let identity = make_test_identity();
    let injection = identity.prompt_injection();
    assert!(injection.contains("TestOak"));
    assert!(injection.contains("Trees"));
    assert!(injection.contains("Prioritize type-safety"));
    assert!(injection.contains("Concise, Technical"));
    assert!(injection.contains("Refactoring"));
}

#[test]
fn test_prompt_injection_empty_directives() {
    let identity = SeedIdentity::new("Minimal".to_string(), "Fungi".to_string());
    let injection = identity.prompt_injection();
    assert!(injection.contains("Minimal"));
    assert!(injection.contains("Fungi"));
    assert!(!injection.contains("Directives"));
}

#[test]
fn test_evolve_directives_only() {
    let mut identity = make_test_identity();
    let new_directives = vec!["Always use Result.".to_string()];
    identity.evolve(Some(new_directives.clone()), None).unwrap();
    assert_eq!(identity.directives.general, new_directives);
    assert!(identity.traits.tone.is_some());
}

#[test]
fn test_evolve_traits_only() {
    let mut identity = make_test_identity();
    let mut new_traits = HashMap::new();
    new_traits.insert("tone".to_string(), "Friendly".to_string());
    new_traits.insert("focus".to_string(), "Debugging".to_string());
    identity.evolve(None, Some(new_traits)).unwrap();
    assert_eq!(identity.traits.tone, Some("Friendly".to_string()));
    assert_eq!(identity.traits.focus, Some("Debugging".to_string()));
}

#[test]
fn test_evolve_clears_empty_traits() {
    let mut identity = make_test_identity();
    let mut new_traits = HashMap::new();
    new_traits.insert("tone".to_string(), String::new());
    identity.evolve(None, Some(new_traits)).unwrap();
    assert!(identity.traits.tone.is_none());
}

#[test]
fn test_evolve_validates_size() {
    let mut identity = make_test_identity();
    let huge_directives = vec!["x".repeat(MAX_IDENTITY_SIZE); 5];
    let result = identity.evolve(Some(huge_directives), None);
    assert!(result.is_err());
}

#[test]
fn test_identity_path_constructs_correctly() {
    let path = identity_path("my-seed-123");
    assert!(path.ends_with("my-seed-123/identity.toml"));
    assert!(path.to_string_lossy().contains(".canopy"));
    assert!(path.to_string_lossy().contains("seeds"));
}

#[test]
fn test_seeds_dir_points_to_home() {
    let dir = seeds_dir();
    assert!(dir.to_string_lossy().contains(".canopy"));
    assert!(dir.ends_with("seeds"));
}

// ── Filesystem I/O tests ─────────────────────────────────────────────

#[test]
fn save_and_load_seed_roundtrip() {
    let seed_id = "test-save-load-roundtrip";
    let identity = make_test_identity();

    // Clean up any existing seed first
    let _ = remove_seed(seed_id);

    let result = save_seed(seed_id, &identity);
    assert!(result.is_ok());

    let loaded = load_seed(seed_id).unwrap();
    assert_eq!(loaded.name, identity.name);
    assert_eq!(loaded.family, identity.family);
    assert_eq!(loaded.directives.general, identity.directives.general);
    assert_eq!(loaded.traits.tone, identity.traits.tone);
    assert_eq!(loaded.traits.focus, identity.traits.focus);

    // Clean up
    let _ = remove_seed(seed_id);
}

#[test]
fn save_seed_creates_directory() {
    let seed_id = "test-creates-dir";
    let identity = SeedIdentity::new("NewSeed".to_string(), "Fungi".to_string());

    let _ = remove_seed(seed_id);
    save_seed(seed_id, &identity).unwrap();

    let dir = seeds_dir().join(seed_id);
    assert!(dir.exists());
    assert!(dir.join("identity.toml").exists());

    let _ = remove_seed(seed_id);
}

#[test]
fn load_seed_not_found_returns_error() {
    let result = load_seed("nonexistent-seed-id");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
}

#[test]
fn remove_seed_deletes_directory() {
    let seed_id = "test-remove-seed";
    let identity = SeedIdentity::new("ToRemove".to_string(), "Minerals".to_string());

    let _ = remove_seed(seed_id);
    save_seed(seed_id, &identity).unwrap();
    assert!(seeds_dir().join(seed_id).exists());

    remove_seed(seed_id).unwrap();
    assert!(!seeds_dir().join(seed_id).exists());
}

#[test]
fn remove_seed_not_found_returns_error() {
    let result = remove_seed("nonexistent-seed-id");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
}

#[test]
fn list_seeds_returns_created_seed() {
    let seed_id = "test-list-seeds";
    let identity = SeedIdentity::new("Listable".to_string(), "Weather".to_string());

    let _ = remove_seed(seed_id);
    save_seed(seed_id, &identity).unwrap();

    let seeds = list_seeds().unwrap();
    assert!(seeds.contains(&seed_id.to_string()));

    let _ = remove_seed(seed_id);
}

#[test]
fn list_seeds_sorted_alphabetically() {
    // Create two seeds with names that sort in a specific order
    let id_a = "test-alpha-seed";
    let id_b = "test-beta-seed";

    let _ = remove_seed(id_a);
    let _ = remove_seed(id_b);

    save_seed(
        id_a,
        &SeedIdentity::new("Alpha".to_string(), "Trees".to_string()),
    )
    .unwrap();
    save_seed(
        id_b,
        &SeedIdentity::new("Beta".to_string(), "Trees".to_string()),
    )
    .unwrap();

    let seeds = list_seeds().unwrap();
    // Seeds are sorted by ID, not name
    let alpha_pos = seeds.iter().position(|s| s == id_a).unwrap();
    let beta_pos = seeds.iter().position(|s| s == id_b).unwrap();
    assert!(alpha_pos < beta_pos);

    let _ = remove_seed(id_a);
    let _ = remove_seed(id_b);
}

#[test]
fn is_name_unique_returns_true_for_new_name() {
    assert!(is_name_unique("CompletelyNewName", None).unwrap());
}

#[test]
fn is_name_unique_detects_collision() {
    let seed_id = "test-unique-check";
    let identity = SeedIdentity::new("UniqueChecker".to_string(), "Trees".to_string());

    let _ = remove_seed(seed_id);
    save_seed(seed_id, &identity).unwrap();

    // Same name should not be unique
    assert!(!is_name_unique("UniqueChecker", None).unwrap());
    // Case-insensitive
    assert!(!is_name_unique("uniquechecker", None).unwrap());
    // Different name should be unique
    assert!(is_name_unique("DifferentName", None).unwrap());

    let _ = remove_seed(seed_id);
}

#[test]
fn is_name_unique_excludes_self() {
    let seed_id = "test-exclude-self";
    let identity = SeedIdentity::new("SelfCheck".to_string(), "Fungi".to_string());

    let _ = remove_seed(seed_id);
    save_seed(seed_id, &identity).unwrap();

    // Should be unique when excluding itself
    assert!(is_name_unique("SelfCheck", Some(seed_id)).unwrap());

    let _ = remove_seed(seed_id);
}

#[test]
fn resolve_seed_by_name_finds_seed() {
    let seed_id = "test-resolve-by-name";
    let identity = SeedIdentity::new("Resolvable".to_string(), "Trees".to_string());

    let _ = remove_seed(seed_id);
    save_seed(seed_id, &identity).unwrap();

    let found = resolve_seed_by_name("Resolvable").unwrap();
    assert_eq!(found, Some(seed_id.to_string()));

    // Case-insensitive
    let found_lower = resolve_seed_by_name("resolvable").unwrap();
    assert_eq!(found_lower, Some(seed_id.to_string()));

    // Non-existent name returns None
    let not_found = resolve_seed_by_name("NonExistent").unwrap();
    assert!(not_found.is_none());

    let _ = remove_seed(seed_id);
}

#[test]
fn validate_combined_fields_and_size() {
    let identity = make_test_identity();
    assert!(identity.validate().is_ok());

    let mut empty_name = make_test_identity();
    empty_name.name = String::new();
    assert!(empty_name.validate().is_err());

    let mut empty_family = make_test_identity();
    empty_family.family = String::new();
    assert!(empty_family.validate().is_err());
}
