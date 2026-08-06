//! Sidecar recording the registry's own view of each CLI's fields as of the
//! last successful registry refresh (`~/.canopy/registry-baseline.toml`).
//!
//! The refresh merge (`setup_module::registry_fetch::apply_registry_refresh`)
//! needs to tell "the user edited this field" apart from "the registry
//! changed this field out from under an untouched config" -- and the only
//! way to do that reliably is to remember what the registry said last time,
//! rather than guess from the value alone. This sidecar is that memory. It
//! is written on every successful refresh and consulted for nothing else.

use serde::{Deserialize, Serialize};
use std::path::Path;

use super::cli_config::CliConfig;

/// File name of the baseline sidecar, relative to the canopy data directory.
pub const REGISTRY_BASELINE_FILE_NAME: &str = "registry-baseline.toml";

/// The registry's published view of every currently-detected CLI, as of the
/// last successful refresh.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryBaseline {
    #[serde(default)]
    pub clis: Vec<CliConfig>,
}

impl RegistryBaseline {
    /// Load the baseline sidecar. Returns `None` when the file is absent
    /// (no refresh has ever completed) or fails to parse -- both cases mean
    /// there is nothing to compare a local edit against.
    pub fn load(canopy_dir: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(canopy_dir.join(REGISTRY_BASELINE_FILE_NAME)).ok()?;
        toml::from_str(&content).ok()
    }

    /// Persist this baseline, overwriting whatever was there before.
    pub fn save(&self, canopy_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(canopy_dir)?;
        let content = toml::to_string_pretty(self).unwrap_or_default();
        std::fs::write(canopy_dir.join(REGISTRY_BASELINE_FILE_NAME), content)
    }

    /// The baselined view of a single CLI by name, if the last refresh knew
    /// about it.
    pub fn get(&self, name: &str) -> Option<&CliConfig> {
        self.clis.iter().find(|c| c.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_cli(name: &str) -> CliConfig {
        CliConfig {
            name: name.to_string(),
            binary: name.to_string(),
            headless_mode: "--headless".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn load_returns_none_when_file_absent() {
        let dir = TempDir::new().unwrap();
        assert!(RegistryBaseline::load(dir.path()).is_none());
    }

    #[test]
    fn save_and_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let baseline = RegistryBaseline {
            clis: vec![sample_cli("codex"), sample_cli("cursor")],
        };
        baseline.save(dir.path()).unwrap();

        let loaded = RegistryBaseline::load(dir.path()).unwrap();
        assert_eq!(loaded.clis.len(), 2);
        assert_eq!(loaded.get("codex").unwrap().binary, "codex");
        assert!(loaded.get("missing").is_none());
    }

    #[test]
    fn save_overwrites_previous_contents() {
        let dir = TempDir::new().unwrap();
        RegistryBaseline {
            clis: vec![sample_cli("codex")],
        }
        .save(dir.path())
        .unwrap();

        RegistryBaseline {
            clis: vec![sample_cli("cursor")],
        }
        .save(dir.path())
        .unwrap();

        let loaded = RegistryBaseline::load(dir.path()).unwrap();
        assert_eq!(loaded.clis.len(), 1);
        assert_eq!(loaded.clis[0].name, "cursor");
    }

    #[test]
    fn load_returns_none_for_malformed_toml() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(REGISTRY_BASELINE_FILE_NAME),
            "not valid toml {{{",
        )
        .unwrap();
        assert!(RegistryBaseline::load(dir.path()).is_none());
    }

    #[test]
    fn get_finds_by_name() {
        let baseline = RegistryBaseline {
            clis: vec![sample_cli("a"), sample_cli("b")],
        };
        assert_eq!(baseline.get("b").unwrap().name, "b");
    }
}
