//! Core domain models for the canopy daemon.
//!
//! Defines agents, triggers, execution logs, and all supporting types.
//! An agent has an optional trigger — `Cron` for scheduled execution,
//! `Watch` for file-system events. An agent without a trigger exists
//! but won't fire automatically.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Unified Agent model ────────────────────────────────────────────────

/// The type of trigger that causes an agent to execute automatically.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    Cron {
        /// Standard 5-field cron expression.
        schedule_expr: String,
    },
    Watch {
        /// Absolute path to file or directory to watch.
        path: String,
        /// File system events to watch for.
        events: Vec<WatchEvent>,
        /// Debounce window in seconds.
        #[serde(default = "default_debounce")]
        debounce_seconds: u64,
        /// Watch subdirectories recursively.
        #[serde(default)]
        recursive: bool,
    },
    // Future trigger types can be added here:
    // Event { event_type: String, payload_filter: Option<String> },
    // Webhook { url: String, secret: Option<String> },
}

impl Trigger {
    pub fn type_str(&self) -> &'static str {
        match self {
            Trigger::Cron { .. } => "cron",
            Trigger::Watch { .. } => "watch",
        }
    }
}

fn default_debounce() -> u64 {
    2
}

/// An agent row that failed to decode — e.g. a `trigger_config` written
/// directly to SQLite by an external tool that isn't the JSON `Trigger`
/// shape canopy expects. Carries just enough to quarantine and report the
/// row; canopy never guesses at what the malformed data meant.
#[derive(Debug, Clone)]
pub struct CorruptAgent {
    pub id: String,
    pub enabled: bool,
    pub error: String,
}

/// A unified agent — the core entity in canopy.
///
/// An agent can have a trigger (cron schedule or file watcher) or no trigger
/// at all (manual-only execution). The trigger field determines how and when
/// the agent runs automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub prompt: String,
    pub trigger: Option<Trigger>,
    pub cli: Cli,
    pub model: Option<String>,
    pub working_dir: Option<String>,
    pub enabled: bool,
    /// One-shot scheduled enable time. When set and `enabled` is `false`,
    /// the scheduler enables the agent (and clears this field) once
    /// `Utc::now() >= enable_at`.
    pub enable_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Log file path.
    pub log_path: String,
    /// Timeout in minutes for execution locking (default: 15).
    pub timeout_minutes: u32,
    // Cron-specific
    pub expires_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_run_ok: Option<bool>,
    // Watch-specific
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub trigger_count: u64,
}

impl Agent {
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Utc::now() > exp)
    }

    pub fn trigger_type_label(&self) -> &'static str {
        match &self.trigger {
            Some(Trigger::Cron { .. }) => "cron",
            Some(Trigger::Watch { .. }) => "watch",
            None => "manual",
        }
    }

    pub fn schedule_expr(&self) -> Option<&str> {
        match &self.trigger {
            Some(Trigger::Cron { schedule_expr }) => Some(schedule_expr),
            _ => None,
        }
    }

    pub fn watch_path(&self) -> Option<&str> {
        match &self.trigger {
            Some(Trigger::Watch { path, .. }) => Some(path),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn watch_events(&self) -> Option<&[WatchEvent]> {
        match &self.trigger {
            Some(Trigger::Watch { events, .. }) => Some(events),
            _ => None,
        }
    }

    pub fn is_cron(&self) -> bool {
        matches!(&self.trigger, Some(Trigger::Cron { .. }))
    }

    pub fn is_watch(&self) -> bool {
        matches!(&self.trigger, Some(Trigger::Watch { .. }))
    }
}

// ── Shared types ────────────────────────────────────────────────────────

/// File system event types that watchers can respond to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WatchEvent {
    Create,
    Modify,
    Delete,
    Move,
}

impl WatchEvent {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "create" => Some(Self::Create),
            "modify" => Some(Self::Modify),
            "delete" => Some(Self::Delete),
            "move" => Some(Self::Move),
            _ => None,
        }
    }

    pub fn parse_list(event_strs: &[String]) -> Result<Vec<WatchEvent>, String> {
        if event_strs.len() == 1 && event_strs[0].eq_ignore_ascii_case("all") {
            return Ok(vec![Self::Create, Self::Modify, Self::Delete, Self::Move]);
        }

        let mut events = Vec::with_capacity(event_strs.len());
        for s in event_strs {
            match WatchEvent::from_str(s) {
                Some(e) => events.push(e),
                None => {
                    return Err(format!(
                        "Invalid event type '{}'. Must be: create, modify, delete, move, or all",
                        s
                    ));
                }
            }
        }
        if events.is_empty() {
            return Err("At least one event type must be specified".to_string());
        }
        Ok(events)
    }
}

impl std::fmt::Display for WatchEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Create => write!(f, "create"),
            Self::Modify => write!(f, "modify"),
            Self::Delete => write!(f, "delete"),
            Self::Move => write!(f, "move"),
        }
    }
}

/// A CLI platform identifier, backed by the canopy registry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct Cli(pub String);

impl Cli {
    pub fn new(name: impl Into<String>) -> Self {
        Cli(name.into())
    }

    pub fn from_str(s: &str) -> Self {
        if s.is_empty() {
            Cli("opencode".to_string())
        } else {
            Cli(s.to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn command_name(&self) -> String {
        let registry = Self::load_registry();
        registry
            .and_then(|r| r.get(self.as_str()).map(|c| c.binary.clone()))
            .unwrap_or_else(|| self.0.clone())
    }

    /// Registry-driven paste+submit behavior for delivering a prompt to this
    /// platform's interactive session as a submitted message. Falls back to
    /// [`super::cli_config::PasteSubmitSpec::default`] when the platform has
    /// no registry entry or leaves the fields unset.
    pub fn paste_submit_spec(&self) -> super::cli_config::PasteSubmitSpec {
        let registry = Self::load_registry();
        let config = registry.as_ref().and_then(|r| r.get(self.as_str()));
        super::cli_config::PasteSubmitSpec::from_cli_config(config)
    }

    pub fn detect_available() -> Vec<Cli> {
        let Some(registry) = Self::load_registry() else {
            return Vec::new();
        };
        registry
            .available_clis
            .iter()
            .map(|c| Cli::new(&c.name))
            .collect()
    }

    pub fn detect_default() -> Option<Cli> {
        let available = Self::detect_available();
        if available.len() == 1 {
            Some(available.into_iter().next().unwrap())
        } else {
            None
        }
    }

    pub fn resolve(param: Option<&str>) -> Result<Cli, String> {
        match param {
            Some(name) if !name.is_empty() => Ok(Cli::new(name)),
            Some(_) => Err("CLI name must not be empty.".to_string()),
            None => match Cli::detect_default() {
                Some(cli) => {
                    tracing::info!("Auto-detected CLI: {}", cli);
                    Ok(cli)
                }
                None => {
                    let available = Cli::detect_available();
                    if available.is_empty() {
                        Err("No CLI found in the registry. Run 'canopy setup' to detect available CLIs.".to_string())
                    } else {
                        Err(format!(
                            "Multiple CLIs found ({}). Please specify the 'cli' parameter explicitly.",
                            available.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")
                        ))
                    }
                }
            },
        }
    }

    pub fn strategy(&self) -> Box<super::cli_strategy::CliStrategy> {
        // `CANOPY_HOME_OVERRIDE` lets tests point this at a fixture
        // `.canopy/config.toml` without mutating the process-wide `HOME` env
        // var — swapping `HOME` itself raced with concurrently-running
        // tests that shell out to git (which reads the real `HOME` for
        // `user.name`/`user.email`), causing unrelated test failures under
        // `cargo test`'s default parallel execution. Unset in production.
        let home = std::env::var_os("CANOPY_HOME_OVERRIDE")
            .map(std::path::PathBuf::from)
            .or_else(dirs::home_dir)
            .expect("Could not determine home directory");
        let canopy_dir = home.join(".canopy");
        let config = super::canopy_config::CanopyConfig::load(&canopy_dir);

        let cli_config = config.get_cli(self.as_str()).unwrap_or_else(|| {
            panic!(
                "CLI '{}' not found in configuration.\n\
                 Available CLIs: {}\n\
                 Run 'canopy setup' to update the configuration.",
                self.as_str(),
                config.cli_names().join(", ")
            )
        });

        Box::new(super::cli_strategy::CliStrategy {
            binary: cli_config.binary.clone(),
            headless_mode: cli_config.headless_mode.clone(),
            model_flag: cli_config.model_flag.clone(),
            supports_working_dir: cli_config.supports_working_dir,
            working_dir_flag: cli_config.working_dir_flag.clone(),
            env_vars: cli_config.env_vars.clone(),
            prompt_via_stdin: cli_config.prompt_via_stdin,
        })
    }

    fn load_registry() -> Option<super::cli_config::CliRegistry> {
        let home = dirs::home_dir()?;
        let config = super::canopy_config::CanopyConfig::load(&home.join(".canopy"));
        if config.clis.is_empty() {
            return None;
        }
        Some(super::cli_config::CliRegistry {
            version: 2,
            available_clis: config.clis,
        })
    }
}

impl std::fmt::Display for Cli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Status of an execution run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Pending,
    InProgress,
    Success,
    Error,
    Timeout,
    Missed,
}

impl RunStatus {
    pub fn from_str(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "success" => Self::Success,
            "error" => Self::Error,
            "timeout" => Self::Timeout,
            "missed" => Self::Missed,
            _ => Self::Pending,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Success => "success",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Missed => "missed",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Outcome of atomically claiming the right to start a run for an agent
/// (see [`crate::application::ports::RunRepository::try_start_run`]).
///
/// This exists so the "is another run already active" check and the
/// "insert the new run" write happen under the same lock — a bare
/// `get_active_run` followed by a separate `insert_run` leaves a window
/// where two concurrent callers can both see "no active run" and both
/// start an execution for the same agent.
#[derive(Debug, Clone)]
pub enum StartRunOutcome {
    /// No other run was active; the given run was recorded as active.
    Started,
    /// Another run for the same agent is already active; nothing was
    /// inserted.
    AlreadyActive(RunLog),
}

/// Record of a single agent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLog {
    pub id: String,
    pub background_agent_id: String,
    pub status: RunStatus,
    pub trigger_type: TriggerType,
    pub summary: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub timeout_at: Option<DateTime<Utc>>,
}

/// How an agent was triggered.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerType {
    Scheduled,
    Manual,
    Watch,
}

impl TriggerType {
    pub fn from_str(s: &str) -> Self {
        match s {
            "manual" => Self::Manual,
            "watch" => Self::Watch,
            _ => Self::Scheduled,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
            Self::Watch => "watch",
        }
    }
}

impl std::fmt::Display for TriggerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Orientation of a split group panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SplitOrientation {
    Horizontal,
    Vertical,
}

impl SplitOrientation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }

    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "vertical" => Self::Vertical,
            _ => Self::Horizontal,
        }
    }
}

/// A paired view of two terminal/interactive sessions rendered side-by-side.
#[derive(Clone)]
pub struct SplitGroup {
    pub id: String,
    pub orientation: SplitOrientation,
    pub session_a: String,
    pub session_b: String,
    #[allow(dead_code)]
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
