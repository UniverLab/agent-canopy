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
