//! CLI usage statistics — tracks how often each CLI is launched.
//!
//! Persisted in `~/.canopy/usage.json` as a simple `{ "cli_name": count }` map.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Per-CLI launch counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CliUsage {
    /// Map of CLI name → number of times launched.
    pub counts: HashMap<String, u64>,
    /// RFC 3339 timestamp of the first time Canopy was run.
    pub first_run_at: Option<String>,
}

impl CliUsage {
    /// Load usage stats from `~/.canopy/usage.json`. Returns empty if missing.
    pub fn load(canopy_dir: &Path) -> Self {
        let path = canopy_dir.join("usage.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str::<CliUsage>(&content).ok())
            .unwrap_or_default()
    }

    /// Save usage stats to `~/.canopy/usage.json`.
    pub fn save(&self, canopy_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(canopy_dir)?;
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(canopy_dir.join("usage.json"), content)
    }

    /// Ensure `first_run_at` is set. Returns true if it was just initialized.
    pub fn ensure_first_run(&mut self) -> bool {
        if self.first_run_at.is_none() {
            self.first_run_at = Some(chrono::Utc::now().to_rfc3339());
            true
        } else {
            false
        }
    }

    /// Increment the counter for a CLI by name.
    pub fn record(&mut self, cli_name: &str) {
        *self.counts.entry(cli_name.to_string()).or_insert(0) += 1;
    }

    /// Get the usage count for a CLI, defaulting to 0.
    pub fn get(&self, cli_name: &str) -> u64 {
        self.counts.get(cli_name).copied().unwrap_or(0)
    }

    /// Return CLI names sorted by usage count descending.
    #[allow(dead_code)]
    pub fn ranked(&self) -> Vec<(&String, &u64)> {
        let mut pairs: Vec<_> = self.counts.iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(a.1));
        pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_record_and_rank() {
        let mut usage = CliUsage::default();
        usage.record("opencode");
        usage.record("opencode");
        usage.record("kiro");

        assert_eq!(usage.get("opencode"), 2);
        assert_eq!(usage.get("kiro"), 1);
        assert_eq!(usage.get("nonexistent"), 0);

        let ranked = usage.ranked();
        assert_eq!(ranked[0].0, "opencode");
        assert_eq!(ranked[1].0, "kiro");
    }

    #[test]
    fn test_save_and_load() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("mistral");

        usage.save(dir.path()).unwrap();
        let loaded = CliUsage::load(dir.path());
        assert_eq!(loaded.get("mistral"), 1);
    }

    #[test]
    fn test_load_missing_returns_default() {
        let dir = TempDir::new().unwrap();
        let usage = CliUsage::load(dir.path());
        assert!(usage.counts.is_empty());
    }

    #[test]
    fn test_ensure_first_run_sets_timestamp() {
        let mut usage = CliUsage::default();
        assert!(usage.first_run_at.is_none());
        let was_set = usage.ensure_first_run();
        assert!(was_set);
        assert!(usage.first_run_at.is_some());
    }

    #[test]
    fn test_ensure_first_run_idempotent() {
        let mut usage = CliUsage::default();
        usage.ensure_first_run();
        let first_ts = usage.first_run_at.clone();
        let was_set = usage.ensure_first_run();
        assert!(!was_set);
        assert_eq!(usage.first_run_at, first_ts);
    }

    #[test]
    fn test_ranked_empty() {
        let usage = CliUsage::default();
        assert!(usage.ranked().is_empty());
    }

    #[test]
    fn test_ranked_sorts_descending() {
        let mut usage = CliUsage::default();
        usage.record("a");
        usage.record("a");
        usage.record("b");
        usage.record("b");
        usage.record("b");
        usage.record("c");

        let ranked = usage.ranked();
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].0, "b");
        assert_eq!(ranked[0].1, &3);
        assert_eq!(ranked[1].0, "a");
        assert_eq!(ranked[1].1, &2);
        assert_eq!(ranked[2].0, "c");
        assert_eq!(ranked[2].1, &1);
    }

    #[test]
    fn test_record_increments_existing() {
        let mut usage = CliUsage::default();
        usage.record("cli");
        usage.record("cli");
        usage.record("cli");
        assert_eq!(usage.get("cli"), 3);
    }

    #[test]
    fn test_get_returns_zero_for_unknown() {
        let usage = CliUsage::default();
        assert_eq!(usage.get("nonexistent"), 0);
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut usage = CliUsage::default();
        usage.record("opencode");
        usage.record("claude");
        usage.first_run_at = Some("2024-01-01T00:00:00Z".to_string());

        let json = serde_json::to_string(&usage).unwrap();
        let deserialized: CliUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.get("opencode"), 1);
        assert_eq!(deserialized.get("claude"), 1);
        assert_eq!(
            deserialized.first_run_at.as_deref(),
            Some("2024-01-01T00:00:00Z")
        );
    }

    #[test]
    fn test_serde_deserialize_minimal() {
        let json = r#"{"counts":{}}"#;
        let usage: CliUsage = serde_json::from_str(json).unwrap();
        assert!(usage.counts.is_empty());
        assert!(usage.first_run_at.is_none());
    }

    #[test]
    fn test_save_and_load_preserves_all_fields() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("kiro");
        usage.record("kiro");
        usage.first_run_at = Some("2024-06-15T12:00:00Z".to_string());

        usage.save(dir.path()).unwrap();
        let loaded = CliUsage::load(dir.path());
        assert_eq!(loaded.get("kiro"), 2);
        assert_eq!(
            loaded.first_run_at.as_deref(),
            Some("2024-06-15T12:00:00Z")
        );
    }

    #[test]
    fn test_load_corrupted_json_returns_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("usage.json"), "not valid json").unwrap();
        let usage = CliUsage::load(dir.path());
        assert!(usage.counts.is_empty());
    }
}
