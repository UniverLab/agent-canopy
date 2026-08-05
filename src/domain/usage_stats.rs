//! CLI usage statistics — tracks how often each CLI is launched.
//!
//! Persisted in `~/.canopy/usage.toml` as a simple `{ "cli_name": count }` map
//! — TOML like every other hand-inspectable file directly under `~/.canopy`
//! (`config.toml`), even though nothing but canopy itself ever edits it.

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

/// Outcome of [`CliUsage::load_with_status`], distinguishing "nothing to
/// load yet" from "something looks wrong" so callers — and `doctor` — don't
/// conflate a genuine first run with data that went missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLoadStatus {
    /// `usage.toml` parsed cleanly.
    Loaded,
    /// `usage.toml` is missing but the pre-migration `usage.json` still
    /// exists and parsed cleanly — migration hasn't completed yet (e.g. a
    /// daemon still holds the legacy file open) and its data was used.
    LoadedFromLegacy,
    /// Both `usage.toml` and `usage.json` exist and parsed; `usage.json` was
    /// the newer of the two, most likely because an older, not-yet-updated
    /// binary's daemon kept appending counts to the legacy path after
    /// migration ran. Its data was preferred so those counts aren't
    /// silently dropped in favor of the (now stale) `usage.toml`.
    LoadedPreferringNewerLegacy,
    /// Neither file is present, but nothing else on disk suggests canopy
    /// has run here before — a genuine first run, where empty stats are the
    /// correct answer.
    FreshInstall,
    /// Neither file parsed, yet other canopy state on disk shows this isn't
    /// a fresh install — usage data is missing (or corrupt) when it should
    /// exist. Empty stats are still returned so callers can proceed, but
    /// this case is distinct from [`UsageLoadStatus::FreshInstall`] and
    /// should be surfaced (see `doctor`'s legacy/new split check) rather
    /// than silently treated as "never used".
    MissingUnexpectedly,
}

impl CliUsage {
    /// Load usage stats from `~/.canopy/usage.toml`, falling back to the
    /// legacy `usage.json` where relevant. Returns empty stats if neither
    /// file has usable data — see [`UsageLoadStatus`] for why that empty
    /// result happened.
    pub fn load(canopy_dir: &Path) -> Self {
        let (usage, status) = Self::load_with_status(canopy_dir);
        if status == UsageLoadStatus::MissingUnexpectedly {
            tracing::warn!(
                canopy_dir = %canopy_dir.display(),
                "usage.toml is missing but other canopy state exists on disk — \
                 treating usage stats as empty, which may hide real counts"
            );
        }
        usage
    }

    /// Load usage stats along with how the load resolved. See
    /// [`UsageLoadStatus`] for the cases this distinguishes.
    ///
    /// When both `usage.toml` and the legacy `usage.json` exist — which can
    /// linger for a while, since [`migrate_legacy_json`] defers deleting
    /// `usage.json` while another canopy process may still be using it —
    /// the more recently modified file wins. This is deliberate: a daemon
    /// still running the pre-migration binary keeps writing counts to
    /// `usage.json` even after migration completes, so preferring whichever
    /// file was touched last is what keeps the counts from silently
    /// regressing to a stale snapshot.
    pub fn load_with_status(canopy_dir: &Path) -> (Self, UsageLoadStatus) {
        let toml_path = canopy_dir.join("usage.toml");
        let json_path = canopy_dir.join("usage.json");

        let toml_usage = std::fs::read_to_string(&toml_path)
            .ok()
            .and_then(|content| toml::from_str::<CliUsage>(&content).ok());
        let json_usage = std::fs::read_to_string(&json_path)
            .ok()
            .and_then(|content| serde_json::from_str::<CliUsage>(&content).ok());

        match (toml_usage, json_usage) {
            (Some(toml_usage), Some(json_usage)) => {
                if modified_time(&json_path) > modified_time(&toml_path) {
                    (json_usage, UsageLoadStatus::LoadedPreferringNewerLegacy)
                } else {
                    (toml_usage, UsageLoadStatus::Loaded)
                }
            }
            (Some(toml_usage), None) => (toml_usage, UsageLoadStatus::Loaded),
            (None, Some(json_usage)) => (json_usage, UsageLoadStatus::LoadedFromLegacy),
            (None, None) => {
                if toml_path.exists() || json_path.exists() || has_other_canopy_state(canopy_dir) {
                    (Self::default(), UsageLoadStatus::MissingUnexpectedly)
                } else {
                    (Self::default(), UsageLoadStatus::FreshInstall)
                }
            }
        }
    }

    /// Save usage stats to `~/.canopy/usage.toml`.
    pub fn save(&self, canopy_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(canopy_dir)?;
        let content = toml::to_string_pretty(self).unwrap_or_default();
        std::fs::write(canopy_dir.join("usage.toml"), content)
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

/// Modification time of `path`, or `None` if it can't be read — treated as
/// "not newer than anything" by callers comparing two mtimes.
fn modified_time(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether `canopy_dir` shows signs of a prior canopy run, other than the
/// usage-stats files themselves — used to tell a genuine first run apart
/// from usage data that went missing unexpectedly.
fn has_other_canopy_state(canopy_dir: &Path) -> bool {
    [
        "config.toml",
        "background_agents.db",
        "cli_config.json",
        "daemon.pid",
    ]
    .iter()
    .any(|name| canopy_dir.join(name).exists())
}

/// One-time migration of the legacy `usage.json` (JSON) to `usage.toml`.
///
/// Safe to call on every startup: a no-op once `usage.toml` exists. Never
/// loses counts — the new file is written to a temp path and atomically
/// renamed into place *before* the old file is removed, so a crash at any
/// point leaves either `usage.json` or `usage.toml` on disk, never neither.
/// A corrupt or unreadable `usage.json` is left untouched rather than
/// discarded, so a future run (or a human) can still recover it.
///
/// Deleting `usage.json` is deferred while `other_instance_may_be_running`
/// is `true` — the caller (`ensure_data_dir` in `main.rs`) computes this
/// once via `daemon::process::other_instance_may_be_running` before calling
/// in, rather than this module reaching for daemon detection itself: this
/// `domain` module is also compiled standalone (see `examples/rag_search.rs`)
/// without the `daemon` module available. This project rebuilds and re-runs
/// from source while the previously installed binary's daemon (and TUI) are
/// still live, so "an old reader of the legacy path still exists" is the
/// default assumption for this migration, not an edge case. The deferred
/// removal is retried on every later call (i.e. every startup) once the
/// caller reports no such process, so the legacy file doesn't linger
/// forever.
pub fn migrate_legacy_json(canopy_dir: &Path, other_instance_may_be_running: bool) {
    let old_path = canopy_dir.join("usage.json");
    let new_path = canopy_dir.join("usage.toml");

    if new_path.exists() {
        // Migration already completed on a prior run; a leftover old file
        // (e.g. a crash between the rename and the removal below, or an
        // older binary's daemon still writing to the legacy path) is only
        // safe to clean up once nothing else may still be using it.
        if !other_instance_may_be_running {
            let _ = std::fs::remove_file(&old_path);
        }
        return;
    }

    let Ok(content) = std::fs::read_to_string(&old_path) else {
        return;
    };
    let Ok(usage) = serde_json::from_str::<CliUsage>(&content) else {
        return;
    };
    let Ok(toml_content) = toml::to_string_pretty(&usage) else {
        return;
    };

    let tmp_path = canopy_dir.join("usage.toml.tmp");
    if std::fs::write(&tmp_path, toml_content).is_err() {
        return;
    }
    if std::fs::rename(&tmp_path, &new_path).is_err() {
        return;
    }
    if !other_instance_may_be_running {
        let _ = std::fs::remove_file(&old_path);
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
        assert_eq!(loaded.first_run_at.as_deref(), Some("2024-06-15T12:00:00Z"));
    }

    #[test]
    fn test_load_corrupted_toml_returns_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("usage.toml"), "not valid toml {{{").unwrap();
        let usage = CliUsage::load(dir.path());
        assert!(usage.counts.is_empty());
    }

    #[test]
    fn test_save_writes_toml_not_json() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("claude");
        usage.save(dir.path()).unwrap();

        assert!(dir.path().join("usage.toml").exists());
        assert!(!dir.path().join("usage.json").exists());
        let content = std::fs::read_to_string(dir.path().join("usage.toml")).unwrap();
        assert!(content.contains("claude"));
        // A JSON document never parses as TOML unless it's degenerate; this
        // guards against silently regressing back to JSON on save.
        assert!(toml::from_str::<CliUsage>(&content).is_ok());
    }

    // ── migrate_legacy_json ─────────────────────────────────────────────

    #[test]
    fn migrate_converts_json_to_toml_and_removes_old_file() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("opencode");
        usage.record("opencode");
        usage.record("kiro");
        usage.first_run_at = Some("2024-01-01T00:00:00Z".to_string());
        std::fs::write(
            dir.path().join("usage.json"),
            serde_json::to_string_pretty(&usage).unwrap(),
        )
        .unwrap();

        migrate_legacy_json(dir.path(), false);

        assert!(!dir.path().join("usage.json").exists());
        assert!(dir.path().join("usage.toml").exists());
        let loaded = CliUsage::load(dir.path());
        assert_eq!(loaded.get("opencode"), 2);
        assert_eq!(loaded.get("kiro"), 1);
        assert_eq!(loaded.first_run_at.as_deref(), Some("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn migrate_is_idempotent_when_toml_already_exists() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("new-counts");
        usage.save(dir.path()).unwrap();
        // A leftover old file (e.g. from a crash between rename and
        // removal on a prior run) must not clobber the already-migrated
        // usage.toml.
        std::fs::write(dir.path().join("usage.json"), r#"{"counts":{"stale":99}}"#).unwrap();

        migrate_legacy_json(dir.path(), false);

        assert!(!dir.path().join("usage.json").exists());
        let loaded = CliUsage::load(dir.path());
        assert_eq!(loaded.get("new-counts"), 1);
        assert_eq!(loaded.get("stale"), 0);
    }

    #[test]
    fn migrate_noop_when_neither_file_exists() {
        let dir = TempDir::new().unwrap();
        migrate_legacy_json(dir.path(), false);
        assert!(!dir.path().join("usage.json").exists());
        assert!(!dir.path().join("usage.toml").exists());
    }

    #[test]
    fn migrate_leaves_corrupted_old_file_untouched() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("usage.json"), "not valid json").unwrap();

        migrate_legacy_json(dir.path(), false);

        // Never discard data we can't parse: the old file must survive so a
        // later run (or a human) can still recover it.
        assert!(dir.path().join("usage.json").exists());
        assert!(!dir.path().join("usage.toml").exists());
    }

    // ── deferred deletion while a daemon may be running ─────────────────

    #[test]
    fn migrate_defers_deletion_of_old_file_when_daemon_running() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("opencode");
        std::fs::write(
            dir.path().join("usage.json"),
            serde_json::to_string_pretty(&usage).unwrap(),
        )
        .unwrap();

        migrate_legacy_json(dir.path(), true);

        // The migration itself still happens...
        assert!(dir.path().join("usage.toml").exists());
        // ...but the legacy file is left in place because the running
        // daemon might still be reading it. This is the acceptance test
        // from the spec: the file stays readable and the ordering consumer
        // (CliUsage::load) still sees the counts.
        assert!(dir.path().join("usage.json").exists());
        let loaded = CliUsage::load(dir.path());
        assert_eq!(loaded.get("opencode"), 1);
    }

    #[test]
    fn migrate_defers_deletion_of_leftover_legacy_file_when_daemon_running() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("kiro");
        usage.save(dir.path()).unwrap();
        // A leftover usage.json even though usage.toml already exists: as
        // if an older binary's still-running daemon recreated it after a
        // prior migration ran (the observed real-world scenario).
        std::fs::write(dir.path().join("usage.json"), r#"{"counts":{"kiro":2}}"#).unwrap();

        migrate_legacy_json(dir.path(), true);

        assert!(
            dir.path().join("usage.json").exists(),
            "must not delete a legacy file a live daemon might still be using"
        );
    }

    #[test]
    fn migrate_cleans_up_deferred_legacy_file_once_daemon_stops() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("kiro");
        usage.save(dir.path()).unwrap();
        std::fs::write(dir.path().join("usage.json"), r#"{"counts":{"kiro":2}}"#).unwrap();

        migrate_legacy_json(dir.path(), true);
        assert!(
            dir.path().join("usage.json").exists(),
            "deletion should be deferred first"
        );

        // The daemon stops; a later run (i.e. a later startup, since this
        // migration runs on every startup) must actually clean up the
        // legacy file rather than defer forever.
        migrate_legacy_json(dir.path(), false);

        assert!(
            !dir.path().join("usage.json").exists(),
            "deferred cleanup must run on a later, quieter run"
        );
    }

    // ── load_with_status ─────────────────────────────────────────────────

    #[test]
    fn load_with_status_loaded_when_only_toml_present() {
        let dir = TempDir::new().unwrap();
        let mut usage = CliUsage::default();
        usage.record("codex");
        usage.save(dir.path()).unwrap();

        let (loaded, status) = CliUsage::load_with_status(dir.path());
        assert_eq!(status, UsageLoadStatus::Loaded);
        assert_eq!(loaded.get("codex"), 1);
    }

    #[test]
    fn load_with_status_loaded_from_legacy_when_toml_missing() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("usage.json"), r#"{"counts":{"kiro":4}}"#).unwrap();

        let (loaded, status) = CliUsage::load_with_status(dir.path());
        assert_eq!(status, UsageLoadStatus::LoadedFromLegacy);
        assert_eq!(loaded.get("kiro"), 4);
    }

    #[test]
    fn load_with_status_prefers_newer_legacy_when_both_present() {
        let dir = TempDir::new().unwrap();
        let mut toml_usage = CliUsage::default();
        toml_usage.record("codex");
        toml_usage.save(dir.path()).unwrap();

        // The still-running old binary keeps appending to the legacy path
        // after migration, so it ends up newer than the migrated snapshot.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(dir.path().join("usage.json"), r#"{"counts":{"codex":9}}"#).unwrap();

        let (loaded, status) = CliUsage::load_with_status(dir.path());
        assert_eq!(status, UsageLoadStatus::LoadedPreferringNewerLegacy);
        assert_eq!(
            loaded.get("codex"),
            9,
            "the newer legacy counts must not be silently dropped in favor of the stale toml"
        );
    }

    #[test]
    fn load_with_status_prefers_toml_when_it_is_newer_than_legacy() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("usage.json"), r#"{"counts":{"codex":1}}"#).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut toml_usage = CliUsage::default();
        toml_usage.record("codex");
        toml_usage.record("codex");
        toml_usage.save(dir.path()).unwrap();

        let (loaded, status) = CliUsage::load_with_status(dir.path());
        assert_eq!(status, UsageLoadStatus::Loaded);
        assert_eq!(loaded.get("codex"), 2);
    }

    #[test]
    fn load_with_status_fresh_install_when_nothing_present() {
        let dir = TempDir::new().unwrap();
        let (loaded, status) = CliUsage::load_with_status(dir.path());
        assert_eq!(status, UsageLoadStatus::FreshInstall);
        assert!(loaded.counts.is_empty());
    }

    #[test]
    fn load_with_status_missing_unexpectedly_when_other_canopy_state_present() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();

        let (loaded, status) = CliUsage::load_with_status(dir.path());
        assert_eq!(status, UsageLoadStatus::MissingUnexpectedly);
        assert!(loaded.counts.is_empty());
    }

    #[test]
    fn load_with_status_missing_unexpectedly_when_toml_corrupt_and_no_legacy() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("usage.toml"), "not valid toml {{{").unwrap();

        let (_, status) = CliUsage::load_with_status(dir.path());
        assert_eq!(status, UsageLoadStatus::MissingUnexpectedly);
    }
}
