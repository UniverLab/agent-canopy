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
    /// Flag that non-interactively trusts the run's working directory for
    /// this invocation, e.g. mistral's `--trust`. Some harnesses refuse to
    /// load project configuration (including MCP server declarations) from
    /// a directory they haven't been told to trust, and — critically — exit
    /// 0 with empty output instead of failing loudly when that happens
    /// (2026-08-13 `gitkit-composition` incident). `None` for harnesses with
    /// no such concept. Passing this flag is always opt-in per node
    /// (`node.config["trust_workdir"]`), never a default, since trusting a
    /// directory changes what the harness will execute there.
    #[serde(default)]
    pub trust_flag: Option<String>,
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
    /// Declarative template for argv assembly. Whitespace-separated tokens;
    /// each token may contain `{{marker}}` placeholders. Markers:
    /// `{{prompt}}`, `{{model}}`, `{{effort}}`, `{{mcp_config}}`,
    /// `{{session_id}}`, `{{session_flag}}`, `{{workdir}}`. Headless mode
    /// flags (`headless_mode`) are always prepended before the template tokens;
    /// they are not part of the template. When `None`, falls back to the legacy
    /// fixed-order assembly (backward compat for platforms not yet migrated).
    ///
    /// Substitution rules:
    /// - A token containing any unavailable marker is dropped entirely.
    /// - A literal flag token (starts with `-`) immediately followed by a
    ///   dropped token is also dropped (prevents orphan flags).
    /// - A token like `--flag={{marker}}` is dropped as a unit if the marker
    ///   is unavailable (no orphan flag possible).
    /// - A token like `{{model}}[effort={{effort}}]` is dropped if either
    ///   marker is unavailable; if both are available, the result is one
    ///   argv word (no shell reinterpretation).
    #[serde(default)]
    pub invocation_template: Option<String>,
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
    /// Check if this CLI is available in the given PATH.
    ///
    /// Uses the shared resolver (`resolve_binary_in`) so detection can never
    /// disagree with the spawner — there is one resolution path in the
    /// codebase, not two. When `path` is `None`, the current process's PATH
    /// is used.
    pub fn is_available(&self) -> bool {
        self.resolve().is_ok()
    }

    /// Resolve this CLI's binary using the shared resolver, returning the
    /// absolute path and which step of the resolution order matched.
    ///
    /// This is the single resolution primitive for detection; it delegates
    /// to [`super::cli_strategy::resolve_binary_in`] so setup/doctor and the
    /// spawner can never disagree (B40).
    pub fn resolve(
        &self,
    ) -> std::result::Result<
        (std::path::PathBuf, super::cli_strategy::ResolutionStep),
        super::cli_strategy::BinaryResolutionError,
    > {
        let path_value = std::env::var("PATH").unwrap_or_default();
        super::cli_strategy::resolve_binary_in(&self.binary, &path_value)
    }

    /// Resolve this CLI's binary against an explicit PATH string.
    ///
    /// Use this to test resolution under a different PATH (e.g. the
    /// daemon's captured PATH) without mutating the process environment.
    pub fn resolve_against(
        &self,
        path: &str,
    ) -> std::result::Result<
        (std::path::PathBuf, super::cli_strategy::ResolutionStep),
        super::cli_strategy::BinaryResolutionError,
    > {
        super::cli_strategy::resolve_binary_in(&self.binary, path)
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
            trust_flag: None,
            instruction_file: None,
            prompt_via_stdin: false,
            paste_submit_delay_ms: None,
            paste_submit_key: None,
            paste_submit_presses: 1,
            invocation_template: None,
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
    fn resolve_against_finds_binary_on_injected_path() {
        // B40: setup/doctor detection must use the same resolver the
        // spawner uses. Point PATH at a temp dir containing a fake
        // executable and confirm resolve_against finds it there, with no
        // dependency on the developer's real PATH.
        let dir = TempDir::new().unwrap();
        let bin_path = dir.path().join("opencode");
        std::fs::write(&bin_path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config = sample_cli_config();
        let (resolved, step) = config
            .resolve_against(dir.path().to_str().unwrap())
            .unwrap();
        assert_eq!(resolved, bin_path);
        assert_eq!(step, super::super::cli_strategy::ResolutionStep::Path);
    }

    #[test]
    fn resolve_against_reports_every_location_searched_on_failure() {
        let mut config = sample_cli_config();
        config.binary = "canopy-test-fixture-cli-missing".to_string();
        let err = config.resolve_against("/usr/bin:/bin").unwrap_err();
        assert!(err.to_string().contains("canopy-test-fixture-cli-missing"));
        assert!(err.to_string().contains("/usr/bin:/bin"));
    }

    #[test]
    fn resolve_against_absolute_binary_skips_path_search() {
        let mut config = sample_cli_config();
        config.binary = "/nonexistent/somewhere/opencode".to_string();
        let (resolved, step) = config.resolve_against("").unwrap();
        assert_eq!(
            resolved,
            std::path::PathBuf::from("/nonexistent/somewhere/opencode")
        );
        assert_eq!(
            step,
            super::super::cli_strategy::ResolutionStep::AbsolutePath
        );
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

    #[test]
    fn default_paste_submit_presses_is_one() {
        assert_eq!(default_paste_submit_presses(), 1);
    }

    #[test]
    fn cli_config_default_construction() {
        let config = CliConfig::default();
        assert!(config.name.is_empty());
        assert!(config.binary.is_empty());
        assert!(config.headless_mode.is_empty());
        assert!(config.model_flag.is_none());
        assert!(!config.supports_working_dir);
        assert!(config.working_dir_flag.is_none());
        assert!(config.env_vars.is_empty());
        assert!(config.interactive_args.is_none());
        assert!(config.fallback_interactive_args.is_none());
        assert!(config.resume_args.is_none());
        assert!(config.session_list_cmd.is_none());
        assert!(config.session_resume_cmd.is_none());
        assert!(config.session_id_set_flag.is_none());
        assert!(config.session_list_format_args.is_none());
        assert!(config.session_id_pattern.is_none());
        assert!(config.models_list_cmd.is_none());
        assert!(config.accent_color.is_none());
        assert!(config.yolo_flag.is_none());
        assert!(config.trust_flag.is_none());
        assert!(config.instruction_file.is_none());
        assert!(!config.prompt_via_stdin);
        assert!(config.paste_submit_delay_ms.is_none());
        assert!(config.paste_submit_key.is_none());
        // Default derive uses u8::default() = 0, not the serde default fn
        assert_eq!(config.paste_submit_presses, 0);
        assert!(config.invocation_template.is_none());
    }

    #[test]
    fn cli_config_serde_roundtrip() {
        let config = sample_cli_config();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: CliConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "opencode");
        assert_eq!(deserialized.binary, "opencode");
        assert!(deserialized.supports_working_dir);
        assert_eq!(deserialized.model_flag.as_deref(), Some("--model"));
    }

    #[test]
    fn cli_config_deserialize_from_minimal_json() {
        let json = r#"{"name":"test"}"#;
        let config: CliConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "test");
        assert!(config.binary.is_empty());
        assert!(!config.prompt_via_stdin);
        assert_eq!(config.paste_submit_presses, 1);
    }

    #[test]
    fn cli_config_deserialize_from_empty_object() {
        let config: CliConfig = serde_json::from_str("{}").unwrap();
        assert!(config.name.is_empty());
        assert!(config.binary.is_empty());
    }

    #[test]
    fn cli_registry_serde_roundtrip() {
        let mut registry = CliRegistry::new();
        registry.available_clis.push(sample_cli_config());
        let json = serde_json::to_string(&registry).unwrap();
        let deserialized: CliRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.version, 2);
        assert_eq!(deserialized.available_clis.len(), 1);
        assert_eq!(deserialized.available_clis[0].name, "opencode");
    }

    #[test]
    fn cli_registry_get_returns_none_for_missing() {
        let registry = CliRegistry::new();
        assert!(registry.get("anything").is_none());
    }

    #[test]
    fn cli_registry_names_empty() {
        let registry = CliRegistry::new();
        assert!(registry.names().is_empty());
    }

    #[test]
    fn paste_submit_spec_default_values() {
        let spec = PasteSubmitSpec::default();
        assert_eq!(spec.settle, std::time::Duration::from_millis(30));
        assert_eq!(spec.submit_key, b"\r");
        assert_eq!(spec.presses, 1);
    }

    #[test]
    fn paste_submit_spec_from_cli_config_all_overrides() {
        let mut config = sample_cli_config();
        config.paste_submit_delay_ms = Some(100);
        config.paste_submit_key = Some("lf".to_string());
        config.paste_submit_presses = 3;
        let spec = PasteSubmitSpec::from_cli_config(Some(&config));
        assert_eq!(spec.settle, std::time::Duration::from_millis(100));
        assert_eq!(spec.submit_key, b"\n");
        assert_eq!(spec.presses, 3);
    }

    #[test]
    fn cli_config_accent_color_serde_roundtrip() {
        let mut config = sample_cli_config();
        config.accent_color = Some([255, 128, 0]);
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: CliConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.accent_color, Some([255, 128, 0]));
    }

    #[test]
    fn cli_config_accent_color_none_by_default() {
        let config = sample_cli_config();
        assert!(config.accent_color.is_none());
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: CliConfig = serde_json::from_str(&json).unwrap();
        assert!(deserialized.accent_color.is_none());
    }
}
