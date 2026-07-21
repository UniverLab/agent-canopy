//! Registry-driven CLI configuration.
//!
//! All CLI definitions come from the canopy registry (`platforms.json`).
//! During setup, available CLIs are detected and saved to `~/.canopy/cli_config.json`.
//! The executor uses this saved config to build commands dynamically --
//! no hard-coded strategies needed.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Complete CLI definition from the registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub binary: String,
    #[serde(default)]
    pub headless_mode: String,
    #[serde(default)]
    pub model_flag: Option<String>,
    #[serde(default)]
    pub supports_working_dir: bool,
    #[serde(default)]
    pub working_dir_flag: Option<String>,
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,
    /// Arguments to pass when launching in interactive (TUI) mode.
    #[serde(default)]
    pub interactive_args: Option<String>,
    /// Fallback interactive args if the primary mode fails to start (e.g. `kiro-cli --tui` → `kiro-cli chat`).
    #[serde(default)]
    pub fallback_interactive_args: Option<String>,
    /// Arguments to pass when launching in resume mode (most recent session).
    #[serde(default)]
    pub resume_args: Option<String>,
    /// Subcommand/args to run to list sessions, e.g. `"session list"`.
    /// When set, the new-agent dialog shows a canopy-side session picker.
    #[serde(default)]
    pub session_list_cmd: Option<String>,
    /// Flag to resume a specific session by ID, e.g. `"--session"`.
    /// The session ID is appended as the next argument.
    #[serde(default)]
    pub session_resume_cmd: Option<String>,
    /// Flag that SETS the session id when spawning a NEW headless session,
    /// e.g. `"--session-id"` on claude/gemini/qwen/copilot. Canopy mints a
    /// UUID, passes it after this flag, and records it on the loop run so
    /// the session can be resumed later. Preferred capture strategy: the id
    /// is known before the process even starts, so nothing has to be parsed
    /// from output or session listings.
    #[serde(default)]
    pub session_id_set_flag: Option<String>,
    /// Extra args appended to [`session_list_cmd`] to make its output
    /// machine-readable and stable, e.g. `"--format json"` (opencode/mimo/
    /// kilo) or `"--json"` (cn). Used by the list-after-run session id
    /// capture (RS1 phase 2): platforms that cannot set the id at spawn but
    /// can list their sessions get their id diffed out of two list snapshots.
    /// Optional — capture only runs when this and [`session_id_pattern`] are
    /// both set (and [`session_id_set_flag`] is not, which takes precedence).
    ///
    /// [`session_list_cmd`]: Self::session_list_cmd
    /// [`session_id_pattern`]: Self::session_id_pattern
    /// [`session_id_set_flag`]: Self::session_id_set_flag
    #[serde(default)]
    pub session_list_format_args: Option<String>,
    /// Regex applied to the session-list command's stdout to extract session
    /// ids for list-after-run capture (RS1 phase 2). Capture group 1 is the
    /// id when the pattern has one; otherwise the whole match. Kept generic
    /// so nothing platform-specific leaks into Rust — every supported CLI
    /// emits JSON with an `"id"` key, so the shared value
    /// `"id"\s*:\s*"([^"]+)"` works for all of them. Optional; see
    /// [`session_list_format_args`] for when capture runs.
    ///
    /// [`session_list_format_args`]: Self::session_list_format_args
    #[serde(default)]
    pub session_id_pattern: Option<String>,
    /// Subcommand/args that make this CLI print its own available model ids,
    /// one passable id per line (e.g. opencode's `models` → `opencode/big-pickle`,
    /// `opencode-go/glm-5.2`). When set, `agent_models` uses this enumeration as
    /// the authoritative, guaranteed-passable catalog for the platform: each
    /// line is the literal string the model flag accepts, prefix and all —
    /// which models.dev cannot know for a universal gateway (it carries neither
    /// the `provider/model` form the CLI requires nor the gateway's private zen
    /// catalog). Registry-driven so nothing is inferred from the CLI name; the
    /// enumeration is cached like the models.dev catalog and never runs on the
    /// hot path.
    #[serde(default)]
    pub models_list_cmd: Option<String>,
    /// RGB accent color for this CLI's agents in the TUI.
    #[serde(default)]
    pub accent_color: Option<[u8; 3]>,
    /// Flag to pass to disable approval prompts (yolo/autonomous mode).
    #[serde(default)]
    pub yolo_flag: Option<String>,
    /// Path to the custom instructions file (e.g. `.github/copilot-instructions.md`).
    #[serde(default)]
    pub instruction_file: Option<String>,
    /// When true, the composed prompt is written to a temp file and piped in
    /// via stdin instead of being passed as a command-line argument, keeping
    /// argv small and fixed-size regardless of prompt size. Only set this for
    /// CLIs that read the prompt from stdin when none is given as an argument
    /// (e.g. `claude -p`). Defaults to `false` (legacy argv behavior), since
    /// most CLIs require the prompt as a positional argument.
    #[serde(default)]
    pub prompt_via_stdin: bool,
    /// Milliseconds to wait after a prompt-builder paste completes before
    /// writing the submit keystroke. `None` uses the built-in default (see
    /// [`PasteSubmitSpec`]). Set this for harnesses whose bracketed-paste
    /// handling needs longer to settle before it will treat the next
    /// keypress as a distinct Enter rather than folding it into the pasted
    /// text.
    #[serde(default)]
    pub paste_submit_delay_ms: Option<u64>,
    /// Key written to submit a prompt-builder paste: `"cr"` (default) or
    /// `"lf"`.
    #[serde(default)]
    pub paste_submit_key: Option<String>,
    /// Number of times to write the submit keystroke, each after its own
    /// settle delay. Some composers need a second Enter to actually submit
    /// rather than just closing multi-line entry. Defaults to 1.
    #[serde(default = "default_paste_submit_presses")]
    pub paste_submit_presses: u8,
}

fn default_paste_submit_presses() -> u8 {
    1
}

/// Registry-driven paste+submit behavior for delivering a prompt-builder
/// prompt to an interactive session as a SUBMITTED message, not just pending
/// input sitting in the target's input box. Resolved from [`CliConfig`]
/// metadata — add fields there for a harness that needs different behavior;
/// never hardcode a CLI name at a call site to pick this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteSubmitSpec {
    pub settle: std::time::Duration,
    pub submit_key: &'static [u8],
    pub presses: u8,
}

/// Small, constant settle delay between the pasted block and the submit
/// keystroke: long enough that a TUI's bracketed-paste handling has closed
/// out the paste event before the next keypress arrives (so it can't be
/// folded into the pasted text), short enough that sending still feels
/// instant.
const DEFAULT_PASTE_SUBMIT_DELAY_MS: u64 = 30;

impl Default for PasteSubmitSpec {
    fn default() -> Self {
        Self {
            settle: std::time::Duration::from_millis(DEFAULT_PASTE_SUBMIT_DELAY_MS),
            submit_key: b"\r",
            presses: 1,
        }
    }
}

impl PasteSubmitSpec {
    /// Resolve from registry metadata, falling back to the default for any
    /// unset field (and for CLIs with no registry entry at all).
    pub fn from_cli_config(config: Option<&CliConfig>) -> Self {
        let default = Self::default();
        let Some(config) = config else {
            return default;
        };
        Self {
            settle: config
                .paste_submit_delay_ms
                .map(std::time::Duration::from_millis)
                .unwrap_or(default.settle),
            submit_key: match config.paste_submit_key.as_deref() {
                Some("lf") => b"\n",
                _ => default.submit_key,
            },
            presses: config.paste_submit_presses.max(1),
        }
    }
}

/// Persisted CLI configuration for available CLIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliRegistry {
    /// Version of the config format
    pub version: u32,
    /// Available CLIs detected during setup
    pub available_clis: Vec<CliConfig>,
}

impl CliConfig {
    /// Check if this CLI is available in PATH.
    pub fn is_available(&self) -> bool {
        which::which(&self.binary).is_ok()
    }
}

impl CliRegistry {
    /// Create a new registry with the current config version.
    pub fn new() -> Self {
        Self {
            version: 2,
            available_clis: Vec::new(),
        }
    }

    /// Detect which CLIs from a list are available in PATH.
    pub fn detect_available(platforms: &[crate::setup_module::PlatformWithCli]) -> Self {
        let mut registry = Self::new();

        for platform in platforms {
            if let Some(ref cli) = platform.cli {
                if cli.is_available() {
                    registry.available_clis.push(cli.clone());
                }
            }
        }

        registry
    }

    /// Save this configuration to a file.
    #[allow(dead_code)]
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)
    }

    /// Load configuration from a file.
    #[allow(dead_code)]
    pub fn load(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Get a CLI config by name.
    pub fn get(&self, name: &str) -> Option<&CliConfig> {
        self.available_clis.iter().find(|c| c.name == name)
    }

    /// Get all available CLI names.
    #[allow(dead_code)]
    pub fn names(&self) -> Vec<&str> {
        self.available_clis
            .iter()
            .map(|c| c.name.as_str())
            .collect()
    }
}

impl Default for CliRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_cli_config() -> CliConfig {
        CliConfig {
            name: "opencode".to_string(),
            binary: "opencode".to_string(),
            headless_mode: "--headless".to_string(),
            model_flag: Some("--model".to_string()),
            supports_working_dir: true,
            working_dir_flag: Some("--dir".to_string()),
            env_vars: std::collections::HashMap::new(),
            interactive_args: None,
            fallback_interactive_args: None,
            resume_args: None,
            session_list_cmd: None,
            session_resume_cmd: None,
            session_id_set_flag: None,
            session_list_format_args: None,
            session_id_pattern: None,
            models_list_cmd: None,
            accent_color: None,
            yolo_flag: None,
            instruction_file: None,
            prompt_via_stdin: false,
            paste_submit_delay_ms: None,
            paste_submit_key: None,
            paste_submit_presses: 1,
        }
    }

    #[test]
    fn test_cli_registry_new_sets_version() {
        let registry = CliRegistry::new();
        assert_eq!(registry.version, 2);
        assert!(registry.available_clis.is_empty());
    }

    #[test]
    fn test_cli_registry_default() {
        let registry = CliRegistry::default();
        assert_eq!(registry.version, 2);
        assert!(registry.available_clis.is_empty());
    }

    #[test]
    fn test_cli_registry_get_found() {
        let mut registry = CliRegistry::new();
        registry.available_clis.push(sample_cli_config());
        let config = registry.get("opencode");
        assert!(config.is_some());
        assert_eq!(config.unwrap().binary, "opencode");
    }

    #[test]
    fn test_cli_registry_get_not_found() {
        let registry = CliRegistry::new();
        let config = registry.get("nonexistent");
        assert!(config.is_none());
    }

    #[test]
    fn test_cli_registry_names() {
        let mut registry = CliRegistry::new();
        let mut cli1 = sample_cli_config();
        cli1.name = "opencode".to_string();
        let mut cli2 = sample_cli_config();
        cli2.name = "kiro".to_string();
        registry.available_clis.push(cli1);
        registry.available_clis.push(cli2);

        let names = registry.names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"opencode"));
        assert!(names.contains(&"kiro"));
    }

    #[test]
    fn test_cli_registry_save_and_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cli_config.json");

        let mut registry = CliRegistry::new();
        registry.available_clis.push(sample_cli_config());

        registry.save(&path).unwrap();

        let loaded = CliRegistry::load(&path).unwrap();
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.available_clis.len(), 1);
        assert_eq!(loaded.available_clis[0].name, "opencode");
    }

    #[test]
    fn test_cli_registry_load_nonexistent() {
        let path = std::path::Path::new("/nonexistent/path/config.json");
        let loaded = CliRegistry::load(path);
        assert!(loaded.is_none());
    }

    #[test]
    fn test_cli_registry_save_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir
            .path()
            .join("nested")
            .join("dir")
            .join("cli_config.json");

        let registry = CliRegistry::new();
        registry.save(&path).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn paste_submit_spec_defaults_when_no_registry_entry() {
        let spec = PasteSubmitSpec::from_cli_config(None);
        assert_eq!(spec, PasteSubmitSpec::default());
        assert_eq!(spec.submit_key, b"\r");
        assert_eq!(spec.presses, 1);
    }

    #[test]
    fn paste_submit_spec_defaults_when_fields_unset() {
        let config = sample_cli_config();
        let spec = PasteSubmitSpec::from_cli_config(Some(&config));
        assert_eq!(spec, PasteSubmitSpec::default());
    }

    #[test]
    fn paste_submit_spec_honors_delay_override() {
        let mut config = sample_cli_config();
        config.paste_submit_delay_ms = Some(80);
        let spec = PasteSubmitSpec::from_cli_config(Some(&config));
        assert_eq!(spec.settle, std::time::Duration::from_millis(80));
    }

    #[test]
    fn paste_submit_spec_honors_lf_key_override() {
        let mut config = sample_cli_config();
        config.paste_submit_key = Some("lf".to_string());
        let spec = PasteSubmitSpec::from_cli_config(Some(&config));
        assert_eq!(spec.submit_key, b"\n");
    }

    #[test]
    fn paste_submit_spec_unrecognized_key_falls_back_to_cr() {
        let mut config = sample_cli_config();
        config.paste_submit_key = Some("bogus".to_string());
        let spec = PasteSubmitSpec::from_cli_config(Some(&config));
        assert_eq!(spec.submit_key, b"\r");
    }

    #[test]
    fn paste_submit_spec_honors_double_enter_override() {
        let mut config = sample_cli_config();
        config.paste_submit_presses = 2;
        let spec = PasteSubmitSpec::from_cli_config(Some(&config));
        assert_eq!(spec.presses, 2);
    }

    #[test]
    fn paste_submit_spec_clamps_zero_presses_to_one() {
        let mut config = sample_cli_config();
        config.paste_submit_presses = 0;
        let spec = PasteSubmitSpec::from_cli_config(Some(&config));
        assert_eq!(spec.presses, 1);
    }
}
