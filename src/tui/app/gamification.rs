//! Gamification hooks on `App`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::application::ports::StateRepository;
use crate::domain::canopy_config::CanopyConfig;
use crate::domain::usage_stats::CliUsage;
use crate::tui::agent::AgentStatus;
use crate::tui::gamification::{MissionEvent, MissionManager, MissionSnapshot};

use super::types::App;

const GARDENER_EDITS_KEY: &str = "gamification:gardener_edits";
const IDENTITY_EVOLVED_KEY: &str = "gamification:identity_evolved";
const DEEP_RAG_SEARCH_KEY: &str = "gamification:deep_rag_search";
const DIGITAL_ARCHEOLOGIST_KEY: &str = "gamification:digital_archeologist";
const MAX_CPU_FREQ_KEY: &str = "gamification:max_cpu_freq_mhz";
const ACCUMULATED_UPTIME_KEY: &str = "gamification:accumulated_uptime_secs";
/// Persist accumulated uptime in batches, not on every frame.
const UPTIME_PERSIST_INTERVAL_SECS: u64 = 30;

/// Detect primary language from marker files at project root.
pub fn detect_project_language(path: &Path) -> Option<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("Cargo.toml", "rust"),
        ("go.mod", "go"),
        ("package.json", "javascript"),
        ("pnpm-lock.yaml", "javascript"),
        ("requirements.txt", "python"),
        ("pyproject.toml", "python"),
        ("Gemfile", "ruby"),
        ("pom.xml", "java"),
        ("build.gradle", "kotlin"),
        ("CMakeLists.txt", "cpp"),
        ("mix.exs", "elixir"),
    ];
    for (file, lang) in MARKERS {
        if path.join(file).exists() {
            return Some(lang);
        }
    }
    None
}

impl App {
    pub(super) fn init_mission_manager(db: Arc<crate::db::Database>) -> Result<MissionManager> {
        MissionManager::load(db)
    }

    pub fn queue_mission_event(&mut self, event: MissionEvent) {
        self.mission_pending_events.push(event);
    }

    pub fn record_gardener_edit(&mut self) -> Result<()> {
        let current = self
            .db
            .get_state(GARDENER_EDITS_KEY)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        self.db
            .set_state(GARDENER_EDITS_KEY, &(current + 1).to_string())?;
        Ok(())
    }

    /// True when the session was launched in yolo mode. Checks both the
    /// literal "yolo" and the CLI's configured yolo flag, since most
    /// harnesses spell it differently (e.g. `--dangerously-skip-permissions`,
    /// `--trust-all-tools`).
    pub(super) fn session_args_contain_yolo(&self, session_id: &str, cli: &str) -> bool {
        let Some(args) = self
            .db
            .get_interactive_session_args(session_id)
            .ok()
            .flatten()
        else {
            return false;
        };

        if args.contains("yolo") {
            return true;
        }

        let canopy_dir = dirs::home_dir()
            .map(|h| h.join(".canopy"))
            .unwrap_or_default();
        CanopyConfig::load(&canopy_dir)
            .clis
            .iter()
            .find(|c| c.name == cli)
            .and_then(|c| c.yolo_flag.as_deref())
            .is_some_and(|flag| !flag.trim().is_empty() && args.contains(flag.trim()))
    }

    /// Total accumulated Canopy runtime: persisted total plus the not yet
    /// persisted seconds of the current anchor window.
    pub(crate) fn accumulated_uptime_secs(&self) -> u64 {
        let stored: u64 = self
            .db
            .get_state(ACCUMULATED_UPTIME_KEY)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let pending = self
            .uptime_anchor
            .map(|anchor| anchor.elapsed().as_secs())
            .unwrap_or(0);
        stored + pending
    }

    /// Fold the current anchor window into the persisted uptime total.
    fn tick_uptime_persist(&mut self) {
        let now = std::time::Instant::now();
        let Some(anchor) = self.uptime_anchor else {
            self.uptime_anchor = Some(now);
            return;
        };

        let pending = now.duration_since(anchor).as_secs();
        if pending < UPTIME_PERSIST_INTERVAL_SECS {
            return;
        }

        let total = self.accumulated_uptime_secs();
        let _ = self
            .db
            .set_state(ACCUMULATED_UPTIME_KEY, &total.to_string());
        self.uptime_anchor = Some(now);
    }

    pub(super) fn tick_missions(&mut self) -> Result<()> {
        let events: Vec<_> = self.mission_pending_events.drain(..).collect();
        let snapshot = self.build_mission_snapshot()?;
        let unlocked =
            self.mission_manager
                .process_refresh(&snapshot, &events, &mut self.whimsg)?;
        for title in &unlocked {
            crate::domain::notification::send_notification(
                title,
                "Mission unlocked",
                crate::domain::notification::NotificationLevel::Success,
            );
        }
        Ok(())
    }

    fn build_mission_snapshot(&mut self) -> Result<MissionSnapshot> {
        let canopy_dir = dirs::home_dir()
            .map(|h| h.join(".canopy"))
            .unwrap_or_default();
        let config = CanopyConfig::load(&canopy_dir);
        let registered_harnesses = registered_harness_names(&config);
        let harnesses_used_count = harnesses_used_count(&self.cli_usage, &registered_harnesses);

        let mut languages = std::collections::HashSet::new();
        for project in &self.projects {
            if let Some(lang) = detect_project_language(Path::new(&project.path)) {
                languages.insert(lang);
            }
        }

        let gardener_edits = self
            .db
            .get_state(GARDENER_EDITS_KEY)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        // One-shot flags written by the daemon (MCP tools run there, not in
        // the TUI process); consumed once via the shared DB.
        let daemon_flags = [
            (IDENTITY_EVOLVED_KEY, MissionEvent::IdentityEvolved),
            (DEEP_RAG_SEARCH_KEY, MissionEvent::DeepRagSearch),
            (
                DIGITAL_ARCHEOLOGIST_KEY,
                MissionEvent::DigitalArcheologistFind,
            ),
        ];
        for (key, event) in daemon_flags {
            if self.db.get_state(key)?.as_deref() == Some("1") {
                // Commit the "done" write before queueing the event: if it
                // fails (e.g. DB contention), leave the flag at "1" and
                // retry the capture next tick instead of double-firing.
                match self.db.set_state(key, "done") {
                    Ok(()) => self.queue_mission_event(event),
                    Err(e) => tracing::warn!(
                        "Failed to consume daemon flag '{key}', will retry next tick: {e}"
                    ),
                }
            }
        }

        let freq = self.system_info.cpu_frequency_mhz;
        if let Some(f) = freq {
            let stored = self
                .db
                .get_state(MAX_CPU_FREQ_KEY)?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let max = stored.max(f);
            if max > stored {
                if let Err(e) = self.db.set_state(MAX_CPU_FREQ_KEY, &max.to_string()) {
                    tracing::warn!("Failed to persist max CPU frequency: {e}");
                }
            }
            self.max_cpu_frequency_seen = Some(max);
        } else {
            self.max_cpu_frequency_seen = self
                .db
                .get_state(MAX_CPU_FREQ_KEY)?
                .and_then(|v| v.parse().ok());
        }

        self.tick_uptime_persist();
        let gpu = self.system_info.gpu_info.as_ref();
        let agent_running = self.has_active_agent_work();

        // "Multitasker" requires concurrent sessions, so count the live ones,
        // not the lifetime total stored in the sessions table.
        let concurrent_sessions = self
            .interactive_agents
            .iter()
            .filter(|a| a.status == AgentStatus::Running)
            .count() as i64;

        Ok(MissionSnapshot {
            canopy_uptime_secs: self.accumulated_uptime_secs(),
            interactive_sessions: concurrent_sessions,
            registered_harness_count: registered_harnesses.len(),
            harnesses_used_count,
            intelligence_nodes: self.db.count_intelligence_nodes().unwrap_or(0),
            cross_project_links: self
                .db
                .count_cross_project_intelligence_links()
                .unwrap_or(0),
            indexed_files: self.rag_info.indexed_files,
            rag_chunks: self.rag_info.total_chunks,
            project_count: self.projects.len() as i64,
            distinct_project_languages: languages.len(),
            gardener_edits,
            loop_node_count_max: self.db.max_loop_nodes_in_any_loop().unwrap_or(0),
            completed_loop_runs: self.db.count_completed_loops().unwrap_or(0),
            total_loop_node_runs: self.db.count_loop_node_runs().unwrap_or(0),
            has_parallel_loop: self.db.has_parallel_loop_run().unwrap_or(false),
            seed_count: crate::domain::seeds::list_seeds()
                .map(|s| s.len())
                .unwrap_or(0),
            max_seed_session_bindings: self.db.max_seed_session_bindings().unwrap_or(0),
            agent_running,
            cpu_usage: self.system_info.cpu_usage,
            cpu_temperature_c: self.system_info.cpu_temperature,
            cpu_frequency_mhz: freq,
            max_cpu_frequency_seen: self.max_cpu_frequency_seen,
            process_count: self.system_info.process_count,
            swap_used_bytes: self.system_info.swap_used,
            gpu_vram_used: gpu.and_then(|g| g.vram_used),
            gpu_vram_total: gpu.and_then(|g| g.vram_total),
        })
    }

    fn has_active_agent_work(&self) -> bool {
        !self.active_runs.is_empty()
            || self
                .interactive_agents
                .iter()
                .any(|a| a.status == AgentStatus::Running)
            || self
                .terminal_agents
                .iter()
                .any(|a| a.status == AgentStatus::Running)
    }
}

fn registered_harness_names(config: &CanopyConfig) -> Vec<String> {
    if !config.clis.is_empty() {
        return config.clis.iter().map(|c| c.name.clone()).collect();
    }
    crate::domain::models::Cli::detect_available()
        .iter()
        .map(|cli| cli.as_str().to_string())
        .collect()
}

/// Distinct *registered* harnesses with at least one recorded launch.
/// Stale usage entries for unregistered CLIs must not count toward
/// "use every registered harness".
fn harnesses_used_count(usage: &CliUsage, registered: &[String]) -> usize {
    registered
        .iter()
        .filter(|name| usage.counts.get(*name).copied().unwrap_or(0) > 0)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harnesses_used_ignores_unregistered_clis() {
        let mut usage = CliUsage::default();
        usage.record("claude");
        usage.record("removed-cli");

        let registered = vec!["claude".to_string(), "gemini".to_string()];
        assert_eq!(harnesses_used_count(&usage, &registered), 1);
    }

    #[test]
    fn harnesses_used_requires_all_registered_for_master() {
        let mut usage = CliUsage::default();
        usage.record("claude");
        usage.record("gemini");

        let registered = vec!["claude".to_string(), "gemini".to_string()];
        assert_eq!(harnesses_used_count(&usage, &registered), registered.len());
    }
}
