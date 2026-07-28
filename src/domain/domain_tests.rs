//! Tests for domain module core functionality

use crate::domain::models::Cli;

#[test]
fn test_cli_resolution() {
    let cli = Cli::resolve(Some("opencode")).unwrap();
    assert_eq!(cli.as_str(), "opencode");

    let cli = Cli::resolve(Some("kiro")).unwrap();
    assert_eq!(cli.as_str(), "kiro");
}

#[test]
fn test_cli_display() {
    let cli = Cli::new("opencode");
    assert_eq!(format!("{}", cli), "opencode");
}

#[test]
fn test_cli_from_str() {
    let cli = Cli::from_str("opencode");
    assert_eq!(cli.as_str(), "opencode");

    let cli = Cli::from_str("kiro");
    assert_eq!(cli.as_str(), "kiro");
}

#[test]
fn test_cli_as_str() {
    let cli = Cli::new("opencode");
    assert_eq!(cli.as_str(), "opencode");
}

#[test]
fn test_cli_new() {
    let cli = Cli::new("test-cli");
    assert_eq!(cli.as_str(), "test-cli");

    let cli = Cli::new(String::from("another-cli"));
    assert_eq!(cli.as_str(), "another-cli");
}
